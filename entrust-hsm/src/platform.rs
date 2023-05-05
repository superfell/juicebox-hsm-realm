use alloc::{format, vec::Vec};
use core::{ops::Sub, slice};

use super::seelib::{
    Cmd_GenerateRandom, Cmd_NVMemOp, M_ByteBlock, M_Cmd_GenerateRandom_Args, M_Command, M_FileID,
    M_NVMemOpType_Write_OpVal, M_Reply, M_Word, NVMemOpType_Read, NVMemOpType_Write,
    SEElib_FreeReply, SEElib_Transact, Status_OK,
};
use hsmcore::hal::{Clock, IOError, NVRam, Nanos, MAX_NVRAM_SIZE};

/// NCipher implements the Platform trait, which provides platform specific
/// functionality to the hsmcore library.
#[derive(Clone)]
pub struct NCipher;

impl rand_core::CryptoRng for NCipher {}

// TODO: This RNG is slow, so we should be using it to seed another one
// instead.
impl rand_core::RngCore for NCipher {
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        let mut cmd = M_Command {
            cmd: Cmd_GenerateRandom,
            ..M_Command::default()
        };
        cmd.args.generaterandom = M_Cmd_GenerateRandom_Args {
            lenbytes: dest.len() as M_Word,
        };
        unsafe {
            let mut reply = M_Reply::default();
            let rc = SEElib_Transact(&mut cmd, &mut reply);
            assert_eq!(0, rc);
            assert_eq!(cmd.cmd, reply.cmd);
            let d = reply.reply.generaterandom.data.as_slice();
            dest.copy_from_slice(d);
            SEElib_FreeReply(&mut reply);
        }
    }

    fn next_u32(&mut self) -> u32 {
        rand_core::impls::next_u32_via_fill(self)
    }

    fn next_u64(&mut self) -> u64 {
        rand_core::impls::next_u64_via_fill(self)
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl M_ByteBlock {
    pub unsafe fn as_slice(&self) -> &[u8] {
        slice::from_raw_parts(self.ptr, self.len as usize)
    }
}

#[derive(Debug, Default, Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
#[repr(C)]
pub struct TimeSpec {
    sec: i32,
    nsec: i32,
}
impl Sub for TimeSpec {
    type Output = Nanos;

    #[allow(clippy::manual_range_contains)] // clippy thinks (0..1_000_000_000).contains(&nsec) is clearer. clippy is nuts.
    fn sub(self, rhs: Self) -> Self::Output {
        if rhs > self {
            Nanos(0)
        } else {
            let mut sec = self.sec - rhs.sec;
            let mut nsec = self.nsec - rhs.nsec;
            if nsec < 0 {
                sec -= 1;
                nsec += 1_000_000_000;
            }
            assert!(sec >= 0);
            assert!(nsec >= 0 && nsec < 1_000_000_000);
            let nanos = (sec as u32)
                .saturating_mul(1_000_000_000)
                .saturating_add(nsec as u32);
            Nanos(nanos)
        }
    }
}

type ClockId = isize;
const CLOCK_MONOTONIC: ClockId = 1;

extern "C" {
    fn clock_gettime(c: ClockId, tm: *mut TimeSpec) -> isize;
}

impl Clock for NCipher {
    type Instant = TimeSpec;

    fn now(&self) -> Option<TimeSpec> {
        let mut tm = TimeSpec::default();
        unsafe {
            match clock_gettime(CLOCK_MONOTONIC, &mut tm) {
                0 => Some(tm),
                _ => None,
            }
        }
    }

    fn elapsed(&self, start: TimeSpec) -> Option<Nanos> {
        Some(self.now()? - start)
    }
}

const NVRAM_FILENAME: M_FileID = M_FileID {
    // state
    bytes: [b's', b't', b'a', b't', b'e', 0, 0, 0, 0, 0, 0],
};

const NCIPHER_NVRAM_LEN: usize = 4096;
const NVRAM_LEN_OFFSET: usize = NCIPHER_NVRAM_LEN - 4;

impl NVRam for NCipher {
    // The admin needs to allocate an nvram area called 'state' with a size of
    // 4096 bytes. The nvram-sw tool can do this.
    // /opt/nfast/bin/nvram-sw --alloc -b 4096 -n state
    //
    // For production we need something that will correctly set the acl on this
    // nvram file.
    //
    // read will always return the full 4096 bytes, and writes need to send a
    // full 4096 bytes. The last 4 bytes hold the size of the data that was
    // written. This is extracted during read to correctly size the returned
    // data.

    fn read(&self) -> Result<Vec<u8>, IOError> {
        let mut cmd = M_Command {
            cmd: Cmd_NVMemOp,
            ..M_Command::default()
        };
        cmd.args.nvmemop.op = NVMemOpType_Read;
        cmd.args.nvmemop.name = NVRAM_FILENAME;

        let mut reply = M_Reply::default();
        unsafe {
            let rc = SEElib_Transact(&mut cmd, &mut reply);
            if rc != 0 {
                return Err(IOError(format!(
                    "SEElib_Transact for NVMemOp read failed with status code {rc}"
                )));
            }
        }
        if cmd.cmd != reply.cmd {
            return Err(IOError(format!(
                "SEElib_Transact reply indicates error {reply:?}"
            )));
        }
        let result: Result<Vec<u8>, IOError> = {
            if reply.status == Status_OK {
                let mut data = unsafe { reply.reply.nvmemop.res.read.data.as_slice().to_vec() };
                // The first read after the NVRam entry was initialized will be
                // all zeros. Which conveniently says the length is zero.
                if data.len() != NCIPHER_NVRAM_LEN {
                    return Err(IOError(format!("data read from NVRam wrong size, should be {NCIPHER_NVRAM_LEN} bytes, but was {}", data.len())));
                }
                let len = u32::from_be_bytes(
                    data[NVRAM_LEN_OFFSET..NVRAM_LEN_OFFSET + 4]
                        .try_into()
                        .unwrap(),
                );
                data.truncate(len as usize);
                Ok(data)
            } else {
                Err(IOError(format!(
                    "error reading from NVRAM: {}",
                    reply.status
                )))
            }
        };
        unsafe {
            SEElib_FreeReply(&mut reply);
        }
        result
    }

    fn write(&self, mut data: Vec<u8>) -> Result<(), IOError> {
        if data.len() > MAX_NVRAM_SIZE {
            return Err(IOError(format!(
                "data with {} bytes is larger than allowed maximum of {MAX_NVRAM_SIZE}",
                data.len()
            )));
        }
        let len = (data.len() as u32).to_be_bytes();
        data.resize(NVRAM_LEN_OFFSET, 0);
        data.extend(&len);

        let mut cmd = M_Command {
            cmd: Cmd_NVMemOp,
            ..M_Command::default()
        };
        cmd.args.nvmemop.op = NVMemOpType_Write;
        cmd.args.nvmemop.name = NVRAM_FILENAME;
        cmd.args.nvmemop.val.write = M_NVMemOpType_Write_OpVal {
            data: M_ByteBlock {
                len: data.len() as M_Word,
                ptr: data.as_ptr() as *mut u8,
            },
        };

        let mut reply = M_Reply::default();
        unsafe {
            let rc = SEElib_Transact(&mut cmd, &mut reply);
            assert_eq!(0, rc);
        }
        assert_eq!(cmd.cmd, reply.cmd);
        let result = if reply.status == Status_OK {
            Ok(())
        } else {
            Err(IOError(format!("error {}", reply.status)))
        };

        unsafe {
            SEElib_FreeReply(&mut reply);
        }
        result
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn elapsed_zero() {
        let s = TimeSpec {
            sec: 123,
            nsec: 4_444_000,
        };
        assert_eq!(Nanos::ZERO, s - s)
    }

    #[test]
    fn elapsed() {
        let s = TimeSpec {
            sec: i32::MAX,
            nsec: 10_000,
        };
        let e = TimeSpec {
            sec: i32::MAX,
            nsec: 100_000,
        };
        assert_eq!(Nanos(100_000 - 10_000), e - s);

        let s = TimeSpec {
            sec: 4,
            nsec: 10_000,
        };
        let e = TimeSpec {
            sec: 6,
            nsec: 100_000,
        };
        assert_eq!(Nanos(2_000_090_000), e - s);
    }

    #[test]
    fn elapsed_nsec_rollover() {
        let s = TimeSpec {
            sec: 10,
            nsec: 900_000_000,
        };
        let e = TimeSpec { sec: 11, nsec: 50 };
        assert_eq!(Nanos(100_000_050), e - s);
    }

    #[test]
    fn end_lt_start() {
        let s = TimeSpec { sec: 5, nsec: 5000 };
        let e = TimeSpec { sec: 5, nsec: 4999 };
        assert_eq!(Nanos::ZERO, e - s);

        let s = TimeSpec { sec: 5, nsec: 5000 };
        let e = TimeSpec { sec: 4, nsec: 9000 };
        assert_eq!(Nanos::ZERO, e - s);
    }

    #[test]
    fn saturates() {
        let s = TimeSpec { sec: 5, nsec: 0 };
        let e = TimeSpec { sec: 1000, nsec: 0 };
        assert_eq!(Nanos::MAX, e - s);

        let e = TimeSpec {
            sec: i32::MAX,
            nsec: 999_999_999,
        };
        assert_eq!(Nanos::MAX, e - s);
    }
}