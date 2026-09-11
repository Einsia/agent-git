//! A closed child input reports an I/O error without changing output-pipe signal behavior.

use std::io;
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;

pub(super) async fn write(stdin: &mut ChildStdin, line: &[u8]) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;
        const F_SETNOSIGPIPE: libc::c_int = 73;
        if unsafe { libc::fcntl(stdin.as_raw_fd(), F_SETNOSIGPIPE, 1) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    let mut operation = std::pin::pin!(async {
        stdin.write_all(line).await?;
        stdin.flush().await
    });
    std::future::poll_fn(|cx| {
        // The mask is restored before yielding, even if the future later moves to another thread.
        #[cfg(all(unix, not(target_vendor = "apple")))]
        let mut signals = match PipeSignals::block() {
            Ok(signals) => signals,
            Err(error) => return std::task::Poll::Ready(Err(error)),
        };
        let result = operation.as_mut().poll(cx);
        #[cfg(all(unix, not(target_vendor = "apple")))]
        {
            signals.consume = matches!(&result, std::task::Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::BrokenPipe);
        }
        result
    })
    .await
}

#[cfg(all(unix, not(target_vendor = "apple")))]
struct PipeSignals {
    mask: libc::sigset_t,
    previous: libc::sigset_t,
    was_pending: bool,
    consume: bool,
}

#[cfg(all(unix, not(target_vendor = "apple")))]
impl PipeSignals {
    fn block() -> io::Result<Self> {
        unsafe {
            let mut mask = std::mem::zeroed();
            let mut previous = std::mem::zeroed();
            libc::sigemptyset(&mut mask);
            libc::sigaddset(&mut mask, libc::SIGPIPE);
            let error = libc::pthread_sigmask(libc::SIG_BLOCK, &mask, &mut previous);
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error));
            }
            let mut pending = std::mem::zeroed();
            if libc::sigpending(&mut pending) != 0 {
                let error = io::Error::last_os_error();
                libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
                return Err(error);
            }
            Ok(Self {
                mask,
                previous,
                was_pending: libc::sigismember(&pending, libc::SIGPIPE) == 1,
                consume: false,
            })
        }
    }
}

#[cfg(all(unix, not(target_vendor = "apple")))]
impl Drop for PipeSignals {
    fn drop(&mut self) {
        unsafe {
            if self.consume && !self.was_pending {
                let timeout = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                };
                // Only a signal created during this poll is consumed; existing pending state survives.
                while libc::sigtimedwait(&self.mask, std::ptr::null_mut(), &timeout) == -1 {
                    if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                        break;
                    }
                }
            }
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut());
        }
    }
}
