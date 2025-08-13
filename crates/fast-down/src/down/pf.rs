#![allow(dead_code)]

use crate::url_info::UrlInfo;
use std::num::NonZeroUsize;

pub struct PfData {
    info: UrlInfo,
    concurrent: Option<NonZeroUsize>,
}
