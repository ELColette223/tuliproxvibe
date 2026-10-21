use crate::model::macros;
use shared::model::ProxyConfigDto;

#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl ProxyConfig {
    /// Returns a safe display string: `scheme://host[:port]` — never includes the password.
    pub fn display_host(&self) -> String {
        url::Url::parse(&self.url).map_or_else(
            |_| self.url.clone(),
            |u| {
                let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
                format!("{}://{}{}", u.scheme(), u.host_str().unwrap_or("?"), port)
            },
        )
    }

    /// Returns a safe log line: `scheme://host[:port] (user: <username>)` or without user part.
    pub fn display_for_log(&self) -> String {
        let host = self.display_host();
        if let Some(user) = &self.username {
            format!("{host} (user: {user})")
        } else {
            host
        }
    }
}

macros::from_impl!(ProxyConfig);
impl From<&ProxyConfigDto> for ProxyConfig {
    fn from(dto: &ProxyConfigDto) -> Self {
        Self { url: dto.url.clone(), username: dto.username.clone(), password: dto.password.clone() }
    }
}

impl From<&ProxyConfig> for ProxyConfigDto {
    fn from(dto: &ProxyConfig) -> Self {
        Self { url: dto.url.clone(), username: dto.username.clone(), password: dto.password.clone() }
    }
}
