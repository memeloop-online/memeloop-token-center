// An owned replay file, never a durable archive acknowledgement.
pub(crate) struct LocalBodyFile {
    #[cfg(target_os = "linux")]
    file: std::fs::File,
    #[cfg(not(target_os = "linux"))]
    file: tempfile::NamedTempFile,
}

impl LocalBodyFile {
    pub(crate) fn new(directory: &std::path::Path) -> std::io::Result<Self> {
        #[cfg(target_os = "linux")]
        let file = tempfile::tempfile_in(directory)?;
        #[cfg(not(target_os = "linux"))]
        let file = tempfile::NamedTempFile::new_in(directory)?;
        Ok(Self { file })
    }

    pub(crate) fn writer(&self) -> std::io::Result<std::fs::File> {
        #[cfg(target_os = "linux")]
        {
            self.file.try_clone()
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.file.reopen()
        }
    }

    pub(crate) fn reopen(&self) -> std::io::Result<std::fs::File> {
        // Reopen an owned descriptor, not a pathname supplied by a client.
        // Each reader has an independent offset; the Linux file is anonymous
        // so a killed gateway leaves no request-body orphan on its volume.
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            std::fs::File::open(format!("/proc/self/fd/{}", self.file.as_raw_fd()))
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.file.reopen()
        }
    }
}
