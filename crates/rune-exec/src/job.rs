//! Windows job objects: stopping a command stops every process it started.
//!
//! Windows has no process groups to signal, and walking the tree from the
//! command's own process misses a child whose parent has already exited. A job
//! holds the whole tree regardless of parentage, and closing its last handle
//! ends every process still in it.

use std::io;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::process::Child;

use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject,
    SetInformationJobObject, TerminateJobObject,
};

/// Most process ids read back from a job at once.
const LISTED_PROCESSES: usize = 256;

/// A job object whose processes end when it is terminated or closed.
///
/// The handle is owned, so it is closed with `CloseHandle` when the job drops,
/// which ends what is left in the job.
#[derive(Debug)]
pub struct Job(OwnedHandle);

impl Job {
    /// Creates an anonymous job that ends its processes when closed.
    pub fn new() -> io::Result<Self> {
        // SAFETY: Null attributes and a null name ask for an unnamed job with
        // the default security descriptor; nothing is read through them.
        #[allow(unsafe_code)]
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: The handle was just returned by CreateJobObjectW, is valid,
        // and is owned by nothing else, so it is closed exactly once.
        #[allow(unsafe_code)]
        let job = Self(unsafe { OwnedHandle::from_raw_handle(handle) });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
            .map_err(|_| io::Error::other("job limit structure is too large"))?;
        // SAFETY: The job handle is live, and the pointer names a structure of
        // exactly the size passed, for the information class that expects it.
        #[allow(unsafe_code)]
        let set = unsafe {
            SetInformationJobObject(
                job.0.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size,
            )
        };
        if set == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    /// Puts a started child, and everything it starts from now on, in the job.
    pub fn assign(&self, child: &Child) -> io::Result<()> {
        // SAFETY: Both handles are live for the call: the job is owned here and
        // the child's process handle is owned by `child`, borrowed for it.
        #[allow(unsafe_code)]
        let assigned =
            unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), child.as_raw_handle()) };
        if assigned == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Ends every process in the job, returning whether the call succeeded.
    pub fn terminate(&self) -> bool {
        // SAFETY: The job handle is live for the call; exit code 1 is reported
        // for each process the job ends.
        #[allow(unsafe_code)]
        let ended = unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) };
        ended != 0
    }

    /// Returns the ids of the processes in the job, at most a fixed number.
    pub fn process_ids(&self) -> io::Result<Vec<u32>> {
        #[repr(C)]
        struct List {
            header: JOBOBJECT_BASIC_PROCESS_ID_LIST,
            rest: [usize; LISTED_PROCESSES],
        }
        let mut list = List {
            header: JOBOBJECT_BASIC_PROCESS_ID_LIST::default(),
            rest: [0; LISTED_PROCESSES],
        };
        let size = u32::try_from(size_of::<List>())
            .map_err(|_| io::Error::other("job process list is too large"))?;
        // SAFETY: The job handle is live, and the pointer names a writable
        // buffer of exactly the size passed, laid out as the header followed by
        // room for more ids, which is the shape this information class fills.
        #[allow(unsafe_code)]
        let queried = unsafe {
            QueryInformationJobObject(
                self.0.as_raw_handle(),
                JobObjectBasicProcessIdList,
                (&raw mut list).cast(),
                size,
                std::ptr::null_mut(),
            )
        };
        if queried == 0 {
            return Err(io::Error::last_os_error());
        }
        let listed = list.header.NumberOfProcessIdsInList as usize;
        Ok(list
            .header
            .ProcessIdList
            .iter()
            .chain(list.rest.iter())
            .take(listed)
            .map(|id| *id as u32)
            .collect())
    }
}
