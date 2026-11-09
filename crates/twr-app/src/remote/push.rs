//! Direct APNs/FCM delivery for self-built apps; credential-free payload recording in debug.
use super::auth;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;

/// Every push says only this: what changed is read from the hub once the app opens.
const TITLE: &str = "twr";
const BODY: &str = "Your agents have an update";

#[derive(Default, Deserialize)]
pub struct Config {
    pub apns: Option<Apns>,
    pub fcm_service_account: Option<PathBuf>,
}
#[derive(Deserialize)]
pub struct Apns {
    pub key_file: PathBuf,
    pub key_id: String,
    pub team_id: String,
    pub topic: String,
    #[serde(default)]
    pub sandbox: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Recipient {
    pub device: String,
    pub platform: String,
    pub token: String,
    pub workspace: String,
}
pub struct Push {
    pub gateway_id: String,
    config: Config,
    recipients: Mutex<Vec<Recipient>>,
    path: PathBuf,
    test_log: Option<PathBuf>,
    http: reqwest::Client,
    oauth: Mutex<Option<(u64, String)>>,
    apns_jwt: Mutex<Option<(u64, String)>>,
}
fn private_file(path: &Path) -> Result<Vec<u8>> {
    let meta = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        meta.is_file()
            && !meta.file_type().is_symlink()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o077 == 0,
        "Push credentials must be a private file owned by your user"
    );
    Ok(std::fs::read(path)?)
}
impl Push {
    pub fn new(dir: &Path, config: Option<&Path>, test_log: Option<PathBuf>) -> Result<Arc<Self>> {
        let id_path = dir.join("gateway-id");
        let gateway_id = match std::fs::read_to_string(&id_path) {
            Ok(id) => {
                uuid::Uuid::parse_str(id.trim())?;
                id.trim().to_owned()
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                use std::io::Write;
                let id = uuid::Uuid::new_v4().to_string();
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(id_path)?;
                file.write_all(id.as_bytes())?;
                id
            }
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            test_log.is_none() || cfg!(debug_assertions),
            "Test push recording is available only in debug builds"
        );
        let config = match config {
            Some(path) => serde_json::from_slice(&private_file(path)?)?,
            None => Config::default(),
        };
        let path = dir.join("push-devices.json");
        let recipients = if path.exists() {
            serde_json::from_slice(&private_file(&path)?)?
        } else {
            Vec::new()
        };
        Ok(Arc::new(Self {
            gateway_id,
            config,
            recipients: Mutex::new(recipients),
            path,
            test_log,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            oauth: Mutex::new(None),
            apns_jwt: Mutex::new(None),
        }))
    }
    pub async fn workspaces(&self) -> Vec<String> {
        self.recipients
            .lock()
            .await
            .iter()
            .map(|r| r.workspace.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect()
    }
    async fn remove(&self, recipient: &Recipient) -> Result<()> {
        let mut records = self.recipients.lock().await;
        records.retain(|r| r.device != recipient.device || r.token != recipient.token);
        self.save_recipients(&records)
    }
    pub async fn unregister(&self, device: &str) -> Result<()> {
        let mut records = self.recipients.lock().await;
        records.retain(|r| r.device != device);
        self.save_recipients(&records)
    }
    fn save_recipients(&self, records: &[Recipient]) -> Result<()> {
        let temp = self.path.with_extension(format!("{}.tmp", auth::secret()));
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec(records)?)?;
        file.sync_all()?;
        std::fs::rename(temp, &self.path)?;
        Ok(())
    }
    pub fn status(&self) -> Value {
        json!({"apns":self.config.apns.is_some(),"fcm":self.config.fcm_service_account.is_some(),"local_test":self.test_log.is_some()})
    }
    pub async fn register(&self, recipient: Recipient) -> Result<()> {
        anyhow::ensure!(
            ["ios", "android", "test"].contains(&recipient.platform.as_str()),
            "Unknown push platform"
        );
        anyhow::ensure!(
            !recipient.token.is_empty()
                && recipient.token.len() <= 4096
                && !recipient.token.chars().any(char::is_control),
            "Invalid push token"
        );
        anyhow::ensure!(
            recipient.platform != "test" || self.test_log.is_some(),
            "Test push is disabled"
        );
        let mut recipients = self.recipients.lock().await;
        recipients.retain(|r| r.device != recipient.device || r.workspace != recipient.workspace);
        recipients.push(recipient);
        self.save_recipients(&recipients)
    }
    pub async fn deliver(
        &self,
        auth: &auth::Auth,
        workspace: &str,
        exclude: &std::collections::HashSet<String>,
        due: &[twr_core::notify::Candidate],
    ) -> Result<()> {
        let Some(first) = due.first() else {
            return Ok(());
        };
        let kind = if due.len() > 1 {
            "digest".to_owned()
        } else {
            format!("{:?}", first.event).to_lowercase()
        };
        let key = due
            .iter()
            .map(|c| c.key.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let devices = auth.devices()?;
        let recipients = self.recipients.lock().await.clone();
        for recipient in recipients {
            if recipient.workspace != workspace
                || exclude.contains(&recipient.device)
                || !devices
                    .as_array()
                    .is_some_and(|a| a.iter().any(|d| d["id"] == recipient.device))
            {
                continue;
            }
            let data = json!({"gateway_id":self.gateway_id,"workspace":workspace,"event":kind,"key":auth::digest(&key),"count":due.len().to_string()});
            let group = self.group(workspace);
            if let Some(path) = &self.test_log {
                use std::io::Write;
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(path)?;
                writeln!(
                    file,
                    "{}",
                    json!({"device":recipient.device,"data":data,"group":group,"title":TITLE,"body":BODY})
                )?;
            }
            let result = match recipient.platform.as_str() {
                "ios" => match &self.config.apns {
                    Some(config) => self.apns(config, &recipient.token, &data, &group).await,
                    None => Ok(true),
                },
                "android" => match &self.config.fcm_service_account {
                    Some(path) => self.fcm(path, &recipient.token, &data, &group).await,
                    None => Ok(true),
                },
                _ => Ok(true),
            };
            match result {
                Ok(false) => {
                    self.remove(&recipient).await?;
                }
                Err(error) => crate::gateway_log!(
                    "Push delivery failed for device {}: {error}",
                    recipient.device
                ),
                _ => {}
            }
        }
        Ok(())
    }
    /// One notification group per gateway and workspace, content free: a new push replaces
    /// the one still shown (APNs `apns-collapse-id`, FCM `tag`) and groups with it
    /// (`thread-id`). A hash, so it stays well under APNs' 64-byte collapse-id limit.
    fn group(&self, workspace: &str) -> String {
        let hash = auth::digest(&format!("{}\n{workspace}", self.gateway_id));
        format!("twr-{}", &hash[..32])
    }
    fn apns_headers<'a>(config: &'a Apns, group: &'a str) -> [(&'static str, &'a str); 4] {
        [
            ("apns-topic", config.topic.as_str()),
            ("apns-push-type", "alert"),
            ("apns-priority", "10"),
            ("apns-collapse-id", group),
        ]
    }
    fn apns_payload(data: &Value, group: &str) -> Value {
        json!({"aps":{"alert":{"title":TITLE,"body":BODY},"sound":"default","thread-id":group},"twr":data})
    }
    fn fcm_message(token: &str, data: &Value, group: &str) -> Value {
        let values = data
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_owned()))
            .collect::<std::collections::HashMap<_, _>>();
        json!({"message":{"token":token,"notification":{"title":TITLE,"body":BODY},"data":values,"android":{"priority":"high","notification":{"channel_id":"twr-attention","tag":group}}}})
    }
    async fn apns(&self, config: &Apns, token: &str, data: &Value, group: &str) -> Result<bool> {
        anyhow::ensure!(
            token.len() <= 256 && token.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid APNs device token"
        );
        let mut cached = self.apns_jwt.lock().await;
        if cached
            .as_ref()
            .is_none_or(|(created, _)| auth::now().saturating_sub(*created) >= 2400)
        {
            let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
            header.kid = Some(config.key_id.clone());
            let token = jsonwebtoken::encode(
                &header,
                &json!({"iss":config.team_id,"iat":auth::now()}),
                &jsonwebtoken::EncodingKey::from_ec_pem(&private_file(&config.key_file)?)?,
            )?;
            *cached = Some((auth::now(), token));
        }
        let jwt = cached.as_ref().unwrap().1.clone();
        drop(cached);
        let host = if config.sandbox {
            "api.sandbox.push.apple.com"
        } else {
            "api.push.apple.com"
        };
        let mut request = self
            .http
            .post(format!("https://{host}/3/device/{token}"))
            .version(reqwest::Version::HTTP_2)
            .bearer_auth(jwt);
        for (name, value) in Self::apns_headers(config, group) {
            request = request.header(name, value);
        }
        let response = request
            .json(&Self::apns_payload(data, group))
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::GONE {
            return Ok(false);
        }
        anyhow::ensure!(
            response.status().is_success(),
            "APNs rejected a notification ({})",
            response.status()
        );
        Ok(true)
    }
    async fn fcm(&self, path: &Path, token: &str, data: &Value, group: &str) -> Result<bool> {
        let account: Value = serde_json::from_slice(&private_file(path)?)?;
        let project = account["project_id"]
            .as_str()
            .context("Firebase project ID missing")?;
        anyhow::ensure!(
            project
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "Invalid Firebase project ID"
        );
        let mut oauth = self.oauth.lock().await;
        if oauth
            .as_ref()
            .is_none_or(|(expires, _)| *expires <= auth::now() + 60)
        {
            let key = account["private_key"]
                .as_str()
                .context("Firebase private key missing")?;
            let claims = json!({"iss":account["client_email"],"scope":"https://www.googleapis.com/auth/firebase.messaging","aud":"https://oauth2.googleapis.com/token","iat":auth::now(),"exp":auth::now()+3600});
            let jwt = jsonwebtoken::encode(
                &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
                &claims,
                &jsonwebtoken::EncodingKey::from_rsa_pem(key.as_bytes())?,
            )?;
            let response = self
                .http
                .post("https://oauth2.googleapis.com/token")
                .form(&[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                    ("assertion", jwt.as_str()),
                ])
                .send()
                .await?;
            anyhow::ensure!(
                response.status().is_success(),
                "Firebase authorization failed ({})",
                response.status()
            );
            let response: Value = response.json().await?;
            *oauth = Some((
                auth::now() + response["expires_in"].as_u64().unwrap_or(300),
                response["access_token"]
                    .as_str()
                    .context("Firebase access token missing")?
                    .to_owned(),
            ));
        }
        let access = oauth.as_ref().unwrap().1.clone();
        drop(oauth);
        let response = self
            .http
            .post(format!(
                "https://fcm.googleapis.com/v1/projects/{project}/messages:send"
            ))
            .bearer_auth(access)
            .json(&Self::fcm_message(token, data, group))
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        anyhow::ensure!(
            response.status().is_success(),
            "FCM rejected a notification ({})",
            response.status()
        );
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apns_jwt_signatures_verify_with_ring() {
        use base64::{engine::general_purpose, Engine as _};
        use ring::signature::KeyPair as _;

        let random = ring::rand::SystemRandom::new();
        let private = ring::signature::EcdsaKeyPair::generate_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &random,
        )
        .unwrap();
        let pair = ring::signature::EcdsaKeyPair::from_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            private.as_ref(),
            &random,
        )
        .unwrap();
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            general_purpose::STANDARD.encode(private.as_ref())
        );
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
        header.kid = Some("local-apns-key".into());
        let claims = json!({"iss":"local-team","iat":auth::now()});
        let token = jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_ec_pem(pem.as_bytes()).unwrap(),
        )
        .unwrap();
        let (message, signature) = token.rsplit_once('.').unwrap();
        let signature = general_purpose::URL_SAFE_NO_PAD.decode(signature).unwrap();
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_FIXED,
            pair.public_key().as_ref(),
        )
        .verify(message.as_bytes(), &signature)
        .unwrap();
        assert_eq!(jsonwebtoken::decode_header(&token).unwrap().kid, header.kid);
    }

    #[test]
    fn fcm_jwt_signatures_verify_with_openssl() {
        use base64::{engine::general_purpose, Engine as _};
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let private = dir.path().join("private.pem");
        let public = dir.path().join("public.pem");
        let generated = Command::new("openssl")
            .args([
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:2048",
                "-out",
            ])
            .arg(&private)
            .output()
            .expect("OpenSSL CLI is required for the push-signing interoperability test");
        assert!(
            generated.status.success(),
            "{}",
            String::from_utf8_lossy(&generated.stderr)
        );
        let exported = Command::new("openssl")
            .args(["pkey", "-in"])
            .arg(&private)
            .args(["-pubout", "-out"])
            .arg(&public)
            .output()
            .unwrap();
        assert!(exported.status.success());
        let claims = json!({"iss":"local-test@example.invalid","aud":"https://oauth2.googleapis.com/token","iat":auth::now(),"exp":auth::now()+3600});
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
            &claims,
            &jsonwebtoken::EncodingKey::from_rsa_pem(&std::fs::read(&private).unwrap()).unwrap(),
        )
        .unwrap();
        let (message, signature) = token.rsplit_once('.').unwrap();
        let signature = general_purpose::URL_SAFE_NO_PAD.decode(signature).unwrap();
        let input = dir.path().join("message");
        let signed = dir.path().join("signature");
        std::fs::write(&input, message).unwrap();
        std::fs::write(&signed, signature).unwrap();
        let verified = Command::new("openssl")
            .args(["dgst", "-sha256", "-verify"])
            .arg(&public)
            .arg("-signature")
            .arg(&signed)
            .arg(&input)
            .output()
            .unwrap();
        assert!(
            verified.status.success(),
            "{}",
            String::from_utf8_lossy(&verified.stderr)
        );
        assert_eq!(
            jsonwebtoken::decode_header(&token).unwrap().alg,
            jsonwebtoken::Algorithm::RS256
        );
    }

    #[tokio::test]
    async fn disabling_push_removes_all_workspaces_only_for_that_device() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("push-log.jsonl");
        let push = Push::new(dir.path(), None, Some(log.clone())).unwrap();
        for (device, workspace) in [("a", "default"), ("a", "other"), ("b", "default")] {
            push.register(Recipient {
                device: device.into(),
                workspace: workspace.into(),
                platform: "test".into(),
                token: format!("token-{device}"),
            })
            .await
            .unwrap();
        }
        push.unregister("a").await.unwrap();
        let restored = Push::new(dir.path(), None, Some(log)).unwrap();
        let records = restored.recipients.lock().await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].device, "b");
    }
    #[tokio::test]
    async fn simultaneous_items_form_one_content_free_digest() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = tempfile::tempdir().unwrap();
        let auth = auth::Auth::open(dir.path()).unwrap();
        let code = auth.begin(vec!["read".into()]).unwrap();
        let (device, _) = auth.pair(&code, "test phone").unwrap();
        let log = dir.path().join("push-log.jsonl");
        let push = Push::new(dir.path(), None, Some(log.clone())).unwrap();
        push.register(Recipient {
            device: device.id,
            platform: "test".into(),
            token: "local-test".into(),
            workspace: "default".into(),
        })
        .await
        .unwrap();
        let due: Vec<_> = (0..2)
            .map(|i| twr_core::notify::Candidate {
                event: twr_core::notify::NotifyEvent::Question,
                key: format!("sensitive-key-{i}"),
                thread: "private-thread".into(),
                turn: None,
                attn_id: None,
                who: "private-name".into(),
                text: "private prompt".into(),
                revision: false,
            })
            .collect();
        push.deliver(&auth, "default", &std::collections::HashSet::new(), &due)
            .await
            .unwrap();
        let text = std::fs::read_to_string(log).unwrap();
        assert_eq!(text.lines().count(), 1);
        let record: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(record["data"]["event"], "digest");
        assert_eq!(record["data"]["count"], "2");
        assert_eq!(record["group"], push.group("default"));
        assert!(!text.contains("private"));
        assert!(!text.contains("sensitive"));
    }
    #[test]
    fn pushes_collapse_per_gateway_and_workspace_without_content() {
        let dir = tempfile::tempdir().unwrap();
        let push = Push::new(dir.path(), None, None).unwrap();
        let group = push.group("default");
        // Stable across gateway restarts, distinct per workspace and per gateway.
        assert_eq!(
            Push::new(dir.path(), None, None).unwrap().group("default"),
            group
        );
        assert_ne!(push.group("other"), group);
        let elsewhere = tempfile::tempdir().unwrap();
        assert_ne!(
            Push::new(elsewhere.path(), None, None)
                .unwrap()
                .group("default"),
            group
        );
        assert!(group.len() <= 64 && group.starts_with("twr-"));
        assert!(!group.contains("default") && !group.contains(&push.gateway_id));
        let config = Apns {
            key_file: "/unused".into(),
            key_id: "key".into(),
            team_id: "team".into(),
            topic: "tools.twr.mobile.dev".into(),
            sandbox: true,
        };
        let headers = Push::apns_headers(&config, &group);
        assert!(headers.contains(&("apns-collapse-id", group.as_str())));
        assert!(headers.contains(&("apns-topic", "tools.twr.mobile.dev")));
        let data = json!({"gateway_id":push.gateway_id,"workspace":"default","event":"question","key":"k","count":"1"});
        let payload = Push::apns_payload(&data, &group);
        assert_eq!(payload["aps"]["thread-id"], group.as_str());
        assert_eq!(payload["aps"]["alert"]["body"], BODY);
        let message = Push::fcm_message("token", &data, &group);
        assert_eq!(
            message["message"]["android"]["notification"]["tag"],
            group.as_str()
        );
        assert_eq!(
            message["message"]["android"]["notification"]["channel_id"],
            "twr-attention"
        );
        assert_eq!(message["message"]["data"]["count"], "1");
    }
}
