//! Volume space queries (total and available bytes) for the volume holding a path.

use std::path::Path;

use serde::Serialize;

/// Total and available bytes of a volume; None when the system could not tell.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct VolumeSpace {
    pub total: Option<u64>,
    pub available: Option<u64>,
}

/// Returns the space figures of the volume holding `path`.
pub fn volume_space(path: &Path) -> VolumeSpace {
    VolumeSpace { total: fs4::total_space(path).ok(), available: fs4::available_space(path).ok() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_space_of_temp_volume() {
        let dir = tempfile::tempdir().unwrap();
        let space = volume_space(dir.path());
        assert!(space.total.unwrap() >= space.available.unwrap());
    }
}
