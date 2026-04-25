use serde::{ser::Serializer, Serialize};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[cfg(mobile)]
    #[error(transparent)]
    PluginInvoke(#[from] tauri::plugin::mobile::PluginInvokeError),

    #[error("Audio session not active — call initSession first")]
    SessionNotActive,

    #[error("Audio session already active")]
    SessionAlreadyActive,

    #[error("Audio session setup failed: {0}")]
    SessionSetupFailed(String),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    #[error("Audio operation failed: {0}")]
    OperationFailed(String),
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Error::Io(_) => "IO_ERROR",
            #[cfg(mobile)]
            Error::PluginInvoke(_) => "PLUGIN_INVOKE_ERROR",
            Error::SessionNotActive => "SESSION_NOT_ACTIVE",
            Error::SessionAlreadyActive => "SESSION_ALREADY_ACTIVE",
            Error::SessionSetupFailed(_) => "SESSION_SETUP_FAILED",
            Error::PermissionDenied(_) => "PERMISSION_DENIED",
            Error::OperationFailed(_) => "OPERATION_FAILED",
        }
    }
}

impl Serialize for Error {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeStruct;

        let mut state = serializer.serialize_struct("Error", 2)?;
        state.serialize_field("code", self.code())?;
        state.serialize_field("message", &self.to_string())?;
        state.end()
    }
}
