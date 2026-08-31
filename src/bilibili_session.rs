//! Shared Bilibili authentication state.
//!
//! The TV QR login implementation predates the DLNA caster, so its concrete
//! code remains in `bilibili_caster` for now.  This module is the stable shared
//! API: new casters must import session operations from here rather than
//! depending on the Bilibili caster implementation details.

pub use crate::cast::bilibili_caster::{
    BilibiliCookie, BilibiliSession, clear_session, cookie_header, init_session_dir,
    is_session_expired, load_session, login_qr, save_session,
};

/// Whether a valid persisted account session is available.
pub fn has_valid_session() -> bool {
    load_session().is_some_and(|s| !is_session_expired(&s))
}
