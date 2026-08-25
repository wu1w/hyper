//! In-flight grok OAuth / device-code status. Tokens never enter JSON.

use hyper_loop::CancelFlag;
use serde_json::{json, Value};
use tokio::sync::Mutex;

pub struct OauthFlow {
    snap: Mutex<OauthSnap>,
    cancel: Mutex<Option<CancelFlag>>,
}

struct OauthSnap {
    phase: &'static str,
    kind: &'static str,
    authorize_url: Option<String>,
    user_code: Option<String>,
    verification_uri: Option<String>,
    verification_uri_complete: Option<String>,
    error: Option<String>,
}

impl OauthFlow {
    pub fn new() -> Self {
        Self {
            snap: Mutex::new(OauthSnap {
                phase: "idle",
                kind: "",
                authorize_url: None,
                user_code: None,
                verification_uri: None,
                verification_uri_complete: None,
                error: None,
            }),
            cancel: Mutex::new(None),
        }
    }

    pub async fn json(&self) -> Value {
        let s = self.snap.lock().await;
        json!({
            "ok": true,
            "phase": s.phase,
            "kind": s.kind,
            "authorize_url": s.authorize_url,
            "user_code": s.user_code,
            "verification_uri": s.verification_uri,
            "verification_uri_complete": s.verification_uri_complete,
            "error": s.error,
        })
    }

    pub async fn abort(&self) -> CancelFlag {
        let flag = CancelFlag::new();
        if let Some(old) = self.cancel.lock().await.replace(flag.clone()) {
            old.cancel();
        }
        flag
    }

    pub async fn set_oauth_waiting(&self, authorize_url: String) {
        let mut s = self.snap.lock().await;
        *s = OauthSnap {
            phase: "waiting",
            kind: "oauth",
            authorize_url: Some(authorize_url),
            user_code: None,
            verification_uri: None,
            verification_uri_complete: None,
            error: None,
        };
    }

    pub async fn set_device_waiting(
        &self,
        user_code: String,
        verification_uri: String,
        verification_uri_complete: String,
    ) {
        let mut s = self.snap.lock().await;
        *s = OauthSnap {
            phase: "waiting",
            kind: "device",
            authorize_url: None,
            user_code: Some(user_code),
            verification_uri: Some(verification_uri),
            verification_uri_complete: Some(verification_uri_complete),
            error: None,
        };
    }

    pub async fn succeed(&self) {
        let mut s = self.snap.lock().await;
        s.phase = "ok";
        s.error = None;
        *self.cancel.lock().await = None;
    }

    pub async fn fail(&self, error: String) {
        let mut s = self.snap.lock().await;
        if error.contains("cancelled") {
            s.phase = "idle";
        } else {
            s.phase = "error";
        }
        s.error = Some(error);
        *self.cancel.lock().await = None;
    }
}
