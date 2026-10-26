//! Process-wide credential broker.
//!
//! `but_askpass::try_init` runs once. Its callback turns each Git prompt into
//! an [`AskpassRequest`] the window can poll or subscribe to. `answer_askpass`
//! submits the text through `submit_prompt_response`. A timeout submits `None`,
//! which cancels the prompt. The answer is wrapped in [`Secret`] before any
//! formatting, and that wrapper's `Debug` does not contain the text.
//!
//! Fetch and push against a local file remote do not ask for a password. When
//! the broker is initialized, GitButler still stats an askpass helper before
//! running git. Tests install a stub that exits without speaking the helper
//! protocol. A shell script cannot complete that protocol, so the round trip
//! is driven by `AskpassBroker::submit_prompt` directly.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::{AskpassKind, AskpassRequest, Backend, BridgeError, Result, Secret};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) type PromptListener = Arc<dyn Fn(AskpassRequest) + Send + Sync>;

struct Inner {
    pending: HashMap<String, AskpassRequest>,
    timed_out: Vec<String>,
    timeout: Duration,
}

struct State {
    runtime: tokio::runtime::Runtime,
    inner: Mutex<Inner>,
    listeners: Mutex<Vec<(u64, PromptListener)>>,
    tokens: AtomicU64,
}

static STATE: OnceLock<State> = OnceLock::new();

/// Drop guard that unregisters an askpass listener.
pub struct AskpassWatch {
    unregister: Option<Box<dyn FnOnce() + Send>>,
}

impl AskpassWatch {
    pub(crate) fn new(unregister: impl FnOnce() + Send + 'static) -> Self {
        Self {
            unregister: Some(Box::new(unregister)),
        }
    }
}

impl Drop for AskpassWatch {
    fn drop(&mut self) {
        if let Some(unregister) = self.unregister.take() {
            unregister();
        }
    }
}

impl std::fmt::Debug for AskpassWatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AskpassWatch")
            .field("listening", &self.unregister.is_some())
            .finish()
    }
}

/// Scripted answers for [`FakeBackend`](crate::FakeBackend) prompts.
/// `None` cancels. An empty queue stops, and errors when a prompt is waiting.
#[derive(Debug, Clone)]
pub struct FakeAskpass {
    pub answers: VecDeque<Option<String>>,
}

impl FakeAskpass {
    pub fn scripted(answers: impl IntoIterator<Item = Option<String>>) -> Self {
        Self {
            answers: answers.into_iter().collect(),
        }
    }

    pub fn respond(&mut self, backend: &dyn Backend) -> Result<usize> {
        let pending = backend.askpass_pending()?;
        if pending.is_empty() {
            return Ok(0);
        }
        if self.answers.is_empty() {
            return Err(BridgeError::not_ready(
                "The fake askpass has no scripted answer.",
            ));
        }
        let mut answered = 0;
        for request in pending {
            let Some(answer) = self.answers.pop_front() else {
                break;
            };
            backend.answer_askpass(&request.id, answer)?;
            answered += 1;
        }
        Ok(answered)
    }
}

pub(crate) fn install() -> Result<()> {
    if STATE.get().is_none() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .thread_name("gb-askpass")
            .worker_threads(2)
            .enable_time()
            .build()
            .map_err(|error| {
                BridgeError::internal(format!("Could not start the askpass broker: {error}"))
            })?;
        let _ = STATE.set(State {
            runtime,
            inner: Mutex::new(Inner {
                pending: HashMap::new(),
                timed_out: Vec::new(),
                timeout: DEFAULT_TIMEOUT,
            }),
            listeners: Mutex::new(Vec::new()),
            tokens: AtomicU64::new(1),
        });
    }
    // A second backend in this process keeps the first callback. That callback
    // reads `STATE`, so it stays valid after the first runtime is installed.
    let _ = but_askpass::try_init(on_event);
    Ok(())
}

/// A local file remote never prompts. GitButler still stats this file when the
/// broker is on, and refuses the fetch if it is missing. The stub only has to
/// exist. Do not replace a helper the caller already configured.
pub(crate) fn install_fetch_stub() -> Result<()> {
    let path = std::env::temp_dir().join("gb-bridge-askpass-stub");
    if !path.is_file() {
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").map_err(|error| {
            BridgeError::internal(format!("Could not write the askpass stub: {error}"))
        })?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&path)
            .map_err(|error| {
                BridgeError::internal(format!("Could not read the askpass stub: {error}"))
            })?
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).map_err(|error| {
            BridgeError::internal(format!(
                "Could not mark the askpass stub executable: {error}"
            ))
        })?;
    }
    if std::env::var_os("GITBUTLER_ASKPASS_BIN").is_none() {
        unsafe {
            std::env::set_var("GITBUTLER_ASKPASS_BIN", &path);
        }
    }
    Ok(())
}

pub(crate) fn set_timeout(timeout: Duration) {
    if let Some(state) = STATE.get() {
        state.lock_inner().timeout = timeout;
    }
}

pub(crate) fn pending() -> Result<Vec<AskpassRequest>> {
    let state = state()?;
    Ok(state.lock_inner().pending.values().cloned().collect())
}

pub(crate) fn watch(on_prompt: Box<dyn Fn(AskpassRequest) + Send + Sync>) -> Result<AskpassWatch> {
    let state = state()?;
    let token = state.tokens.fetch_add(1, Ordering::Relaxed);
    state
        .listeners
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push((token, Arc::from(on_prompt)));
    Ok(AskpassWatch::new(move || {
        let Some(state) = STATE.get() else {
            return;
        };
        state
            .listeners
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .retain(|(have, _)| *have != token);
    }))
}

pub(crate) fn answer(id: &str, response: Option<String>) -> Result<()> {
    let state = state()?;
    let id = id.to_string();
    // `block_on` from a `gb-askpass` worker panics. Callers are the UI thread
    // and the timeout task, and the timeout task awaits `finish` directly.
    state.runtime.block_on(finish(&id, response, false))
}

fn state() -> Result<&'static State> {
    STATE
        .get()
        .ok_or_else(|| BridgeError::not_ready("Askpass is not running."))
}

impl State {
    fn lock_inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

fn on_event(event: but_askpass::PromptEvent<but_askpass::Context>) {
    let Some(state) = STATE.get() else {
        return;
    };
    let kind = classify(&event.prompt);
    let request = AskpassRequest {
        id: event.id.to_string(),
        prompt: event.prompt,
        kind,
    };
    let timeout = {
        let mut inner = state.lock_inner();
        inner.pending.insert(request.id.clone(), request.clone());
        inner.timeout
    };
    let id = request.id.clone();
    state.runtime.spawn(async move {
        tokio::time::sleep(timeout).await;
        let _ = finish(&id, None, true).await;
    });
    notify(&request);
}

fn notify(request: &AskpassRequest) {
    let Some(state) = STATE.get() else {
        return;
    };
    let listeners: Vec<_> = state
        .listeners
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .iter()
        .map(|(_, callback)| Arc::clone(callback))
        .collect();
    for listener in listeners {
        listener(request.clone());
    }
}

async fn finish(id: &str, response: Option<String>, from_timeout: bool) -> Result<()> {
    // Wrap before any formatting. `into_inner` happens only at the API call.
    let secret = Secret::new(response);
    let state = state()?;
    {
        let mut inner = state.lock_inner();
        if inner.pending.remove(id).is_none() {
            let timed_out = inner.timed_out.iter().any(|have| have == id);
            return Err(if timed_out {
                BridgeError::not_found("That prompt already timed out.")
            } else {
                BridgeError::not_found("That prompt is no longer waiting.")
            });
        }
        if from_timeout {
            inner.timed_out.push(id.to_string());
            if inner.timed_out.len() > 32 {
                inner.timed_out.remove(0);
            }
        }
    }
    let parsed = id
        .parse()
        .map_err(|_| BridgeError::not_found(format!("Prompt id '{id}' is not a prompt id.")))?;
    but_api::legacy::askpass::submit_prompt_response(
        but_api::legacy::askpass::SubmitPromptResponseParams {
            id: parsed,
            response: secret.into_inner(),
        },
    )
    .await
    .map_err(|error| crate::mutate::api_error("answer the prompt", error))?;
    Ok(())
}

pub(crate) fn classify(prompt: &str) -> AskpassKind {
    let prompt = prompt.to_ascii_lowercase();
    if prompt.contains("passphrase") {
        AskpassKind::Passphrase
    } else if prompt.contains("username") || prompt.contains("password") || prompt.contains("token")
    {
        AskpassKind::Credential
    } else {
        AskpassKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::classify;
    use crate::{AskpassKind, Secret};

    #[test]
    fn secret_debug_redacts_and_kinds_follow_the_prompt() {
        let rendered = format!("{:?}", Secret::new(Some("s3cret".to_string())));
        assert_eq!(rendered, "Secret([redacted])");
        assert!(!rendered.contains("s3cret"));
        assert_eq!(format!("{}", Secret::new(None)), "Secret(None)");
        assert_eq!(
            classify("Password for 'https://example.test': "),
            AskpassKind::Credential
        );
        assert_eq!(
            classify("Enter passphrase for key"),
            AskpassKind::Passphrase
        );
        assert_eq!(
            classify("Username for 'https://example.test': "),
            AskpassKind::Credential
        );
        assert_eq!(classify("Ready to continue?"), AskpassKind::Other);
    }
}
