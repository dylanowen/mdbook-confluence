use confluence::Session;
use semver::Version;

use async_trait::async_trait;

use crate::renderer::Error;

#[async_trait]
pub trait EnhancedSession: Sized {
    async fn get_server_version(&self) -> Result<Version, Error>;
}

#[async_trait]
impl EnhancedSession for Session {
    async fn get_server_version(&self) -> Result<Version, Error> {
        let server_info = self.get_server_info().await?;

        Version::parse(&format!(
            "{}.{}.{}",
            &server_info.major_version, &server_info.minor_version, &server_info.patch_level
        ))
        .map_err(|error| {
            Error::Error(format!(
                "Failed to parse Confluence Version: {} for {:?}",
                error, server_info
            ))
        })
    }
}
