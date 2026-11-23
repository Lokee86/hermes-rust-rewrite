//! Browser file staging for file.attach. The authenticated profile chooses the directory.
use super::{Shared, auth::Access, rest::profile_matches};
use anyhow::{Context, Result, ensure};
use axum::{
    Json,
    extract::{Multipart, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tempfile::NamedTempFile;
use uuid::Uuid;

pub(super) const MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Deserialize)]
pub(super) struct UploadQuery {
    profile: Option<String>,
}

pub(super) async fn receive(
    _: Access,
    State(shared): State<Arc<Shared>>,
    Query(query): Query<UploadQuery>,
    mut multipart: Multipart,
) -> Response {
    let root = shared.library.root().to_owned();
    if !profile_matches(&shared, query.profile.as_deref(), false) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut upload = None;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(e) => return e.into_response(),
        };
        if field.name() != Some("file") || upload.is_some() {
            return StatusCode::BAD_REQUEST.into_response();
        }
        let name = safe_filename(field.file_name().unwrap_or("attachment"));
        let bytes = match field.bytes().await {
            Ok(b) if b.len() <= MAX_BYTES => b,
            Ok(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
            Err(e) => return e.into_response(),
        };
        upload = Some((name, bytes));
    }
    let Some((name, bytes)) = upload else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match tokio::task::spawn_blocking(move || publish(&root, &name, &bytes)).await {
        Ok(Ok(reply)) => Json(reply).into_response(),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not stage this attachment",
        )
            .into_response(),
    }
}

fn safe_filename(value: &str) -> String {
    let filename = value.rsplit(['/', '\\']).next().unwrap_or("");
    let clean = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '-'
                }
            })
            .collect::<String>()
    };
    let path = Path::new(filename);
    let stem = clean(path.file_stem().and_then(|n| n.to_str()).unwrap_or(""));
    let stem = stem.trim_matches(['.', '-']);
    let stem = if stem.is_empty() { "attachment" } else { stem };
    let ext = clean(path.extension().and_then(|n| n.to_str()).unwrap_or(""));
    let ext = ext.trim_matches(['.', '-']);
    let ext = &ext[..ext.len().min(32)];
    let suffix: String = if ext.is_empty() {
        String::new()
    } else {
        format!(".{ext}")
    };
    format!("{}{suffix}", &stem[..stem.len().min(120 - suffix.len())])
}

pub(super) fn install_id(home: &Path) -> Result<String> {
    let root = if home
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|n| n == "profiles")
    {
        home.parent()
            .and_then(Path::parent)
            .context("Invalid profile home")?
    } else {
        home
    };
    let path = root.join("install_id");
    fn read(path: &Path) -> Result<String> {
        let raw = fs::read_to_string(path)?;
        let id = raw
            .trim_start_matches('\u{feff}')
            .trim()
            .to_ascii_lowercase();
        ensure!(
            id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid installation identity"
        );
        Ok(id)
    }
    match fs::symlink_metadata(&path) {
        Ok(meta) => {
            ensure!(meta.is_file(), "Invalid installation identity");
            return read(&path);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let mut file = NamedTempFile::new_in(root)?;
    writeln!(file, "{}", Uuid::new_v4().simple())?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(&path) {
        Ok(_) => {}
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    read(&path)
}

fn uploads(home: &Path, create: bool) -> Result<PathBuf> {
    let dir = home.join("uploads");
    if create {
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.recursive(false).create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    ensure!(
        fs::symlink_metadata(&dir)?.is_dir() && fs::canonicalize(&dir)? == dir,
        "Invalid upload directory"
    );
    Ok(dir)
}

fn publish(root: &Path, name: &str, bytes: &[u8]) -> Result<Value> {
    let home = fs::canonicalize(root)?;
    let identity = install_id(&home)?;
    let dir = uploads(&home, true)?;
    let retention = Duration::from_secs(7 * 24 * 60 * 60);
    for item in fs::read_dir(&dir)?.flatten() {
        if item.file_name().to_string_lossy().starts_with("web-")
            && item.file_type().is_ok_and(|t| t.is_file())
            && item
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > retention)
        {
            let _ = fs::remove_file(item.path());
        }
    }
    let target = dir.join(format!(
        "web-{}-{}",
        &Uuid::new_v4().simple().to_string()[..16],
        name
    ));
    let mut temp = NamedTempFile::new_in(&dir)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(&target)?;
    Ok(
        json!({"ok":true,"size":bytes.len(),"path":target,"staged_upload":{"install_id":identity,"profile_home":home,"path":target}}),
    )
}

pub(super) fn resolve_staged(root: &Path, descriptor: &Value) -> Result<PathBuf> {
    let home = fs::canonicalize(root)?;
    let bound_home = descriptor["profile_home"]
        .as_str()
        .context("Missing upload profile")?;
    ensure!(
        Path::new(bound_home).is_absolute() && fs::canonicalize(bound_home)? == home,
        "Foreign upload profile"
    );
    ensure!(
        descriptor["install_id"].as_str() == Some(&install_id(&home)?),
        "Foreign upload installation"
    );
    let source = Path::new(descriptor["path"].as_str().context("Missing upload path")?);
    let dir = uploads(&home, false)?;
    ensure!(
        source.is_absolute()
            && source
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.starts_with("web-")),
        "Invalid upload path"
    );
    let meta = fs::symlink_metadata(source)?;
    ensure!(
        meta.is_file() && meta.len() <= MAX_BYTES as u64,
        "Invalid staged file"
    );
    let resolved = fs::canonicalize(source)?;
    ensure!(
        resolved.parent() == Some(dir.as_path()),
        "Outside upload directory"
    );
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn staged_receipt_survives_reopen_and_rejects_foreign_sources() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        let upload = publish(home, "notes.txt", b"sample").unwrap();
        let receipt = &upload["staged_upload"];
        assert_eq!(
            fs::read(resolve_staged(home, receipt).unwrap()).unwrap(),
            b"sample"
        );
        let other = tempfile::tempdir().unwrap();
        assert!(resolve_staged(other.path(), receipt).is_err());
        let mut changed = receipt.clone();
        changed["install_id"] = json!("other");
        assert!(resolve_staged(home, &changed).is_err());
        changed = receipt.clone();
        changed["path"] = json!(home.join("install_id"));
        assert!(resolve_staged(home, &changed).is_err());
        assert_eq!(safe_filename(r"..\..\ю.txt"), "attachment.txt");
    }

    #[tokio::test]
    async fn http_upload_requires_the_profile_token_and_returns_a_redeemable_receipt() {
        let root = tempfile::tempdir().unwrap();
        let config = hermes_engine::Config {
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "fixture".into(),
            api_key: String::new(),
        };
        let service =
            super::super::Gateway::start(super::super::GatewayConfig::new(root.path(), config))
                .await
                .unwrap();
        let http = reqwest::Client::new();
        let url = format!("{}/api/chat/file-upload", service.http_url());
        let send = |url: &str| {
            http.post(url).header("content-type","multipart/form-data; boundary=test-boundary")
            .body("--test-boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"note.txt\"\r\n\r\nverified\r\n--test-boundary--\r\n")
        };
        assert_eq!(
            send(&url).send().await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            send(&format!("{url}?profile=other"))
                .bearer_auth(service.token())
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let response: Value = send(&url)
            .bearer_auth(service.token())
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(response["size"], 8);
        assert_eq!(
            fs::read(resolve_staged(root.path(), &response["staged_upload"]).unwrap()).unwrap(),
            b"verified"
        );
        service.shutdown().await.unwrap();
    }
}
