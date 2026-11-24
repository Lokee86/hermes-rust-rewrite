use super::{Core, VERSION};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{
    io::{
        AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
        BufReader,
    },
    process::{Child, ChildStdin, ChildStdout},
    time::timeout,
};
const LIMIT: usize = 16 * 1024 * 1024;
const START: Duration = Duration::from_secs(20);

async fn write_frame(
    w: &mut (impl AsyncWrite + Unpin),
    channel: u32,
    kind: u8,
    data: &[u8],
) -> Result<()> {
    ensure!(data.len() <= LIMIT, "Herdr frame exceeds limit");
    let mut head = [0u8; 9];
    head[..4].copy_from_slice(&channel.to_le_bytes());
    head[4] = kind;
    head[5..].copy_from_slice(&(data.len() as u32).to_le_bytes());
    w.write_all(&head).await?;
    w.write_all(data).await?;
    w.flush().await?;
    Ok(())
}
async fn read_frame(r: &mut (impl AsyncRead + Unpin)) -> Result<(u32, u8, Vec<u8>)> {
    let ch = r.read_u32_le().await?;
    let kind = r.read_u8().await?;
    let len = r.read_u32_le().await? as usize;
    ensure!(len <= LIMIT, "Invalid Herdr frame length");
    let mut data = vec![0; len];
    r.read_exact(&mut data).await?;
    Ok((ch, kind, data))
}
async fn read_line(r: &mut (impl AsyncBufRead + Unpin)) -> Result<Value> {
    let mut data = Vec::new();
    r.take((LIMIT + 1) as u64)
        .read_until(b'\n', &mut data)
        .await?;
    ensure!(
        data.last() == Some(&b'\n') && data.len() <= LIMIT,
        "Incomplete or excessive Herdr response"
    );
    Ok(serde_json::from_slice(&data)?)
}
pub struct Connection {
    _child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    framed: bool,
}
impl Connection {
    pub async fn open(core: &Core, framed: bool) -> Result<Self> {
        let mut child = core
            .command()?
            .arg("connect")
            .spawn()
            .context("Could not start pinned Herdr Core 0.1.22")?;
        let input = child.stdin.take().context("Herdr stdin unavailable")?;
        let output = BufReader::new(child.stdout.take().context("Herdr stdout unavailable")?);
        let mut this = Self {
            _child: child,
            input,
            output,
            framed: false,
        };
        let hello = this
            .call("hello", json!({"generation":1,"framed":framed}))
            .await?;
        ensure!(
            hello["generation"] == 1 && hello["version"] == VERSION,
            "Herdr Core must be {VERSION} (generation 1)"
        );
        this.framed = framed;
        Ok(this)
    }
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let mut bytes =
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))?;
        let reply = timeout(START, async {
            if self.framed {
                write_frame(&mut self.input, 0, 0, &bytes).await?;
                let (channel, kind, body) = read_frame(&mut self.output).await?;
                ensure!(channel == 0 && kind == 0, "Unexpected Herdr RPC frame");
                Ok(serde_json::from_slice(&body)?)
            } else {
                bytes.push(b'\n');
                self.input.write_all(&bytes).await?;
                self.input.flush().await?;
                read_line(&mut self.output).await
            }
        })
        .await
        .context("Herdr RPC timed out")??;
        ensure!(reply["id"] == 1, "Herdr returned an unexpected response");
        if let Some(error) = reply.get("error") {
            bail!(
                "Herdr {method}: {}",
                error["message"].as_str().unwrap_or("request failed")
            )
        }
        reply.get("result").cloned().context("Missing Herdr result")
    }
}

pub enum Frame {
    Bytes(Vec<u8>),
    Closed(Value),
}
pub struct Attach {
    connection: Connection,
    channel: u32,
}
impl Attach {
    pub(super) async fn open(core: &Core, name: &str, cols: u16, rows: u16) -> Result<Self> {
        let mut connection = Connection::open(core, true).await?;
        let result = connection
            .call(
                "terminal.attach",
                json!({"terminal":name,"history":1000,"cols":cols,"rows":rows}),
            )
            .await?;
        let channel = u32::try_from(
            result["channel"]
                .as_u64()
                .context("Missing Herdr channel")?,
        )?;
        ensure!(channel > 0, "Invalid Herdr channel");
        Ok(Self {
            connection,
            channel,
        })
    }
    /// Run the reader in its own task. Partial frames must survive competing viewer input.
    pub fn split(self) -> (Output, Input) {
        let Connection {
            _child,
            input,
            output,
            ..
        } = self.connection;
        (
            Output {
                _child,
                output,
                channel: self.channel,
            },
            Input {
                input,
                channel: self.channel,
            },
        )
    }
}
pub struct Output {
    _child: Child,
    output: BufReader<ChildStdout>,
    channel: u32,
}
pub struct Input {
    input: ChildStdin,
    channel: u32,
}
impl Output {
    pub async fn next(&mut self) -> Result<Frame> {
        loop {
            let (ch, kind, data) = read_frame(&mut self.output).await?;
            ensure!(ch == self.channel, "Unexpected terminal stream channel");
            match kind {
                0 => return Ok(Frame::Bytes(data)),
                1 => continue,
                2 => {
                    return Ok(Frame::Closed(if data.is_empty() {
                        Value::Null
                    } else {
                        serde_json::from_slice(&data)?
                    }));
                }
                _ => bail!("Unexpected Herdr stream frame"),
            }
        }
    }
}
impl Input {
    pub async fn send(&mut self, data: &[u8]) -> Result<()> {
        write_frame(&mut self.input, self.channel, 0, data).await
    }
    pub async fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        let mut size = [0; 4];
        size[..2].copy_from_slice(&cols.to_le_bytes());
        size[2..].copy_from_slice(&rows.to_le_bytes());
        write_frame(&mut self.input, self.channel, 3, &size).await
    }
    pub async fn credit(&mut self, n: usize) -> Result<()> {
        write_frame(
            &mut self.input,
            self.channel,
            1,
            &u32::try_from(n)?.to_le_bytes(),
        )
        .await
    }
}
