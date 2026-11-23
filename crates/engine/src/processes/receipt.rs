//! A claim is durable before spawn. If a gateway loses the acknowledgement,
//! retrying the same tool call returns its result or an unknown disposition.
use anyhow::{Context, Result, ensure};
use ring::{
    digest::{SHA256, digest},
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
struct Claim {
    fingerprint: String,
    process: String,
}

#[derive(Clone)]
pub(super) struct Receipt {
    path: PathBuf,
    pub id: String,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Receipt {
    pub fn claim(
        dir: &Path,
        turn: usize,
        call: &str,
        request: &super::Launch,
    ) -> Result<(Self, bool)> {
        ensure!(
            !call.is_empty(),
            "Terminal call requires a stable tool-call ID"
        );
        let claims = dir.join("process-claims");
        let results = dir.join("process-results");
        fs::create_dir_all(&claims)?;
        fs::create_dir_all(&results)?;
        let key = hex(digest(&SHA256, call.as_bytes()).as_ref());
        let path = claims.join(format!("{turn}-{key}.json"));
        let fingerprint = hex(digest(&SHA256, &serde_json::to_vec(request)?).as_ref());
        let mut nonce = [0; 12];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| anyhow::anyhow!("Process ID unavailable"))?;
        let proposed = Claim {
            fingerprint: fingerprint.clone(),
            process: format!("proc_{}", hex(&nonce)),
        };
        let mut temp = tempfile::NamedTempFile::new_in(&claims)?;
        serde_json::to_writer(temp.as_file_mut(), &proposed)?;
        temp.as_file_mut().flush()?;
        temp.as_file().sync_all()?;
        let (saved, new) = match temp.persist_noclobber(&path) {
            Ok(_) => {
                #[cfg(unix)]
                File::open(&claims)?.sync_all()?;
                (proposed, true)
            }
            Err(e) if e.error.kind() == ErrorKind::AlreadyExists => {
                (serde_json::from_reader(File::open(path)?)?, false)
            }
            Err(e) => return Err(e.error.into()),
        };
        ensure!(
            saved.fingerprint == fingerprint,
            "Conflicting arguments for an existing terminal launch"
        );
        Ok((Self::for_id(dir, &saved.process)?, new))
    }

    pub fn for_id(dir: &Path, id: &str) -> Result<Self> {
        let token = id.strip_prefix("proc_").context("Invalid process ID")?;
        ensure!(
            token.len() == 24 && token.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid process ID"
        );
        Ok(Self {
            path: dir.join("process-results").join(format!("{id}.json")),
            id: id.to_owned(),
        })
    }

    pub fn save(&self, result: &Value) -> Result<()> {
        let parent = self.path.parent().context("Invalid result path")?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(tmp.as_file_mut(), result)?;
        tmp.as_file_mut().flush()?;
        tmp.as_file().sync_all()?;
        tmp.persist(&self.path)
            .context("Could not commit process receipt")?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    }

    pub fn read(&self) -> Result<Option<Value>> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(Some(serde_json::from_reader(file)?))
    }

    pub fn unknown(&self) -> Value {
        json!({"session_id":self.id,"status":"unknown","error":"Previous launch has no confirmed outcome. Inspect its effects before starting another command."})
    }
}
