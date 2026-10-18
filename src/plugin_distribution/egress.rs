use std::{fmt, io::Read, path::Path};

use super::PluginDistributionError;

#[derive(Clone)]
pub struct InstallProxyCredentials {
    username: String,
    password: String,
}

impl fmt::Debug for InstallProxyCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("InstallProxyCredentials([REDACTED])")
    }
}

impl InstallProxyCredentials {
    /// Bounded anonymous pipe input from the host control process.
    pub fn from_stdin() -> Result<Self, PluginDistributionError> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input {
            username: String,
            password: String,
        }
        let invalid = || PluginDistributionError::InvalidEgressProxy;
        let mut bytes = Vec::new();
        std::io::stdin()
            .take(4097)
            .read_to_end(&mut bytes)
            .map_err(|_| invalid())?;
        if bytes.len() > 4096 {
            return Err(invalid());
        }
        let input: Input = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        for value in [&input.username, &input.password] {
            if value.is_empty() || value.len() > 255 || value.chars().any(char::is_control) {
                return Err(invalid());
            }
        }
        Ok(Self {
            username: input.username,
            password: input.password,
        })
    }

    pub fn from_files(username: &Path, password: &Path) -> Result<Self, PluginDistributionError> {
        Ok(Self {
            username: read_proxy_secret(username)?,
            password: read_proxy_secret(password)?,
        })
    }
}

fn read_proxy_secret(path: &Path) -> Result<String, PluginDistributionError> {
    let invalid = || PluginDistributionError::InvalidEgressProxy;
    if !path.is_absolute() {
        return Err(invalid());
    }
    let metadata = std::fs::metadata(path).map_err(|_| invalid())?;
    if !metadata.is_file() || metadata.len() > 257 {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|_| invalid())?
        .take(258)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid())?;
    if bytes.len() > 257 {
        return Err(invalid());
    }
    let value = std::str::from_utf8(&bytes)
        .map_err(|_| invalid())?
        .trim_end_matches(['\r', '\n']);
    if value.is_empty() || value.len() > 255 || value.chars().any(char::is_control) {
        return Err(invalid());
    }
    Ok(value.to_owned())
}

pub(super) fn proxy_url(
    proxy: Option<&str>,
    credentials: Option<&InstallProxyCredentials>,
) -> Result<Option<String>, PluginDistributionError> {
    let Some(credentials) = credentials else {
        return Ok(proxy.map(str::to_owned));
    };
    let invalid = || PluginDistributionError::InvalidEgressProxy;
    let mut proxy = url::Url::parse(proxy.ok_or_else(invalid)?).map_err(|_| invalid())?;
    let encode = |value: &str| {
        url::form_urlencoded::byte_serialize(value.as_bytes())
            .collect::<String>()
            .replace('+', "%20")
    };
    proxy
        .set_username(&encode(&credentials.username))
        .map_err(|_| invalid())?;
    proxy
        .set_password(Some(&encode(&credentials.password)))
        .map_err(|_| invalid())?;
    Ok(Some(proxy.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_distribution::{
        CosignCommandSpec, cosign_proxy_command, cosign_stderr_class,
    };

    #[test]
    fn proxy_credentials_are_bounded_redacted_and_only_in_child_environment() {
        let directory = tempfile::tempdir().unwrap();
        let username = directory.path().join("username");
        let password = directory.path().join("password");
        std::fs::write(&username, "proxy:user@%\n").unwrap();
        std::fs::write(&password, "proxy/pass:#% +\r\n").unwrap();
        let credentials = InstallProxyCredentials::from_files(&username, &password).unwrap();
        assert_eq!(
            format!("{credentials:?}"),
            "InstallProxyCredentials([REDACTED])"
        );
        let proxy = proxy_url(Some("socks5h://100.64.0.2:1080"), Some(&credentials))
            .unwrap()
            .unwrap();
        let parsed = url::Url::parse(&proxy).unwrap();
        assert_eq!(
            percent_encoding::percent_decode_str(parsed.username())
                .decode_utf8()
                .unwrap(),
            "proxy:user@%"
        );
        assert_eq!(
            percent_encoding::percent_decode_str(parsed.password().unwrap())
                .decode_utf8()
                .unwrap(),
            "proxy/pass:#% +"
        );
        let command = CosignCommandSpec {
            arguments: vec![
                "verify".into(),
                "--certificate-identity".into(),
                "reviewed-identity".into(),
            ],
            environment: vec![("DOCKER_CONFIG".into(), "/private/registry".into())],
            current_directory: directory.path().to_owned(),
        };
        let child = cosign_proxy_command(&command, Some(&proxy));
        assert_eq!(child.arguments, command.arguments);
        assert_eq!(child.environment[0], command.environment[0]);
        assert_eq!(
            child.environment[1],
            ("HTTP_PROXY".into(), proxy.clone().into())
        );
        assert_eq!(
            child.environment[2],
            ("HTTPS_PROXY".into(), proxy.clone().into())
        );
        assert_eq!(child.environment[3], ("NO_PROXY".into(), "".into()));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
        let hostile = format!("{proxy} proxy/pass:#% + unexpected EOF");
        assert_eq!(cosign_stderr_class(hostile.as_bytes()), "unclassified");
        assert!(proxy_url(None, Some(&credentials)).is_err());
        assert_eq!(proxy_url(None, None).unwrap(), None);
    }

    #[test]
    fn proxy_secret_files_fail_closed_without_echoing_content_or_paths() {
        let directory = tempfile::tempdir().unwrap();
        let username = directory.path().join("private-user");
        let password = directory.path().join("private-password");
        std::fs::write(&username, "proxy-user").unwrap();
        for value in [
            vec![],
            vec![b'x'; 256],
            vec![0xff],
            b"secret\0tail".to_vec(),
            b"secret\ninside".to_vec(),
        ] {
            std::fs::write(&password, value).unwrap();
            let error = InstallProxyCredentials::from_files(&username, &password).unwrap_err();
            assert!(matches!(error, PluginDistributionError::InvalidEgressProxy));
            assert!(!error.to_string().contains("private-password"));
            assert!(!error.to_string().contains("secret"));
        }
        std::fs::write(&password, vec![b'x'; 255]).unwrap();
        assert!(InstallProxyCredentials::from_files(&username, &password).is_ok());
        assert!(InstallProxyCredentials::from_files(Path::new("relative"), &password).is_err());
        assert!(InstallProxyCredentials::from_files(directory.path(), &password).is_err());
        std::fs::remove_file(&password).unwrap();
        assert!(InstallProxyCredentials::from_files(&username, &password).is_err());
    }
}
