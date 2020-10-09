extern crate nrfs;

use nrfs::*;

pub fn main() {
    let mut memfs = MemFS::default();
    let _ignore = memfs.create("file.test", u64::from(FileModes::S_IRWXU));
    println!("{:?}", memfs);
}
