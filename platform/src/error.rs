//! Typed platform errors. User-facing hints live here (single place), and
//! are safe by construction: ids are numeric, titles never enter messages.

/// Copy shown when the OS denies capture. Points at the exact Settings page.
pub const PERMISSION_HINT: &str = "Sem permissão de Gravação de Tela — autorize em Ajustes → Privacidade e Segurança → Gravação de Tela e tente de novo.";

/// Copy shown when Windows denies capture. Points at Settings, never at a title.
pub const CAMERA_PERMISSION_HINT: &str =
    "Sem permissão de Câmera — autorize em Configurações → Privacidade e segurança → Câmera e tente de novo.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlatformError {
    /// OS denied capture (or hid every source, which is how denial looks).
    PermissionDenied { hint: &'static str },
    /// A previously listed source vanished (display unplugged, window closed).
    SourceGone { id: String },
    /// Valid request the current OS cannot serve.
    UnsupportedPlatform { reason: &'static str },
    /// Feature needs a newer OS than the one running.
    OsVersionTooOld { have: String, need: &'static str },
    /// Malformed selection (empty id, unknown kind).
    InvalidSource { reason: &'static str },
    /// Anything else. Carries redacted upstream text only — never titles,
    /// pixels, or tokens (backends must sanitize before constructing).
    Internal(String),
}

impl PlatformError {
    pub fn permission_denied() -> Self {
        Self::PermissionDenied { hint: PERMISSION_HINT }
    }

    pub fn camera_permission_denied() -> Self {
        Self::PermissionDenied { hint: CAMERA_PERMISSION_HINT }
    }
}

impl std::fmt::Display for PlatformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PermissionDenied { hint } => write!(f, "{hint}"),
            Self::SourceGone { id } => write!(f, "fonte {id} não existe mais"),
            Self::UnsupportedPlatform { reason } => write!(f, "não suportado aqui: {reason}"),
            Self::OsVersionTooOld { have, need } => {
                write!(f, "requer {need} (sistema: {have})")
            }
            Self::InvalidSource { reason } => write!(f, "fonte inválida: {reason}"),
            Self::Internal(detail) => write!(f, "falha de captura: {detail}"),
        }
    }
}

impl std::error::Error for PlatformError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_copy_points_at_settings() {
        let message = PlatformError::permission_denied().to_string();
        assert!(message.contains("Gravação de Tela"));
    }

    #[test]
    fn messages_carry_no_payloads() {
        // Ids are numeric strings; titles must never be formatted in.
        let gone = PlatformError::SourceGone { id: "123".into() }.to_string();
        assert!(gone.contains("123"));
        assert!(!gone.contains("Segredo"));
    }
}
