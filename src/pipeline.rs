use crate::native::{self, Metadata, Ranges};
use std::ffi::{CStr, CString};
use std::sync::Arc;

pub(crate) enum JobPath {
    Full(CString),
    Child { parent: Arc<CString>, name: CString },
}

impl JobPath {
    pub(crate) fn materialize(&self) -> CString {
        match self {
            Self::Full(path) => path.clone(),
            Self::Child { parent, name } => native::join(parent, name.to_bytes()),
        }
    }

    pub(crate) fn in_buffer<'a>(&self, buffer: &'a mut Vec<u8>) -> &'a CStr {
        buffer.clear();
        match self {
            Self::Full(path) => buffer.extend_from_slice(path.to_bytes_with_nul()),
            Self::Child { parent, name } => {
                buffer.extend_from_slice(parent.to_bytes());
                if buffer.last() != Some(&b'/') {
                    buffer.push(b'/');
                }
                buffer.extend_from_slice(name.to_bytes_with_nul());
            }
        }
        CStr::from_bytes_with_nul(buffer).expect("path components contain no NUL")
    }
}

pub(crate) struct Job {
    pub owner: usize,
    pub path: JobPath,
    pub meta: Metadata,
    pub block: i64,
    pub domain: i32,
}
pub(crate) struct Mapped {
    pub job: Job,
    pub mapping: native::FileMapping,
}

pub(crate) fn map(job: Job, buffer: &mut Vec<u8>) -> Mapped {
    let path = job.path.in_buffer(buffer);
    let mapping = if job.meta.allocated > 0 {
        native::map_file(path, job.meta, job.block)
    } else {
        native::FileMapping {
            extents: Ok((Ranges::default(), 0)),
            attributes: if job.meta.extended & native::NO_XATTRS != 0 {
                Ok(false)
            } else {
                native::unknown_xattrs(path)
            },
        }
    };
    Mapped { job, mapping }
}
