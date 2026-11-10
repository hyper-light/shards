#![allow(dead_code)]
use std::{ffi::OsStr,path::{Path,PathBuf},time::Duration,fmt};
pub struct Run {pub status:Option<i32>,pub signal:Option<i32>,pub stdout:String,pub stderr:String,pub elapsed:Duration}
impl fmt::Display for Run {fn fmt(&self,f:&mut fmt::Formatter<'_>)->fmt::Result {write!(f,"light-only")}}
pub struct TempDir(PathBuf);
impl TempDir {pub fn new(_: &str)->Self {panic!("VM helper forbidden in light check")}}
impl std::ops::Deref for TempDir {type Target=Path;fn deref(&self)->&Path {&self.0}}
pub fn test_guest()->&'static Path {Path::new("/LIGHT-ONLY-NO-GUEST")}
pub fn shards()->&'static Path {Path::new("/LIGHT-ONLY-NO-CLI")}
pub fn sha256_digest(_: &[u8])->String {panic!("VM identity helper forbidden in light check")}
pub fn cannot_run_vms()->bool {true}
pub fn run_shards_env<S:AsRef<OsStr>>(_: &[&str],_: &[S],_: &[(&str,&OsStr)],_:Duration)->Run {panic!("VM execution forbidden in light check")}
