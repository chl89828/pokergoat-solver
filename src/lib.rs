//! POKER GOAT 솔버 래퍼 라이브러리.
//!
//! CLI(`src/main.rs`)와 통합 테스트(`tests/`)가 같이 쓴다.
//! 엔진은 postflop-solver(AGPL-3.0-or-later)이고 이 크레이트도 같은 라이선스다.

pub mod aggregate;
pub mod blob;
pub mod cards;
pub mod config;
pub mod export;
pub mod range;
pub mod solve;
pub mod tree;
pub mod validate;

/// blob 헤더에 박히는 솔버 버전. 포맷이나 엔진이 바뀌면 올리고 새 경로에 쓴다 (§5.2).
pub const SOLVER_VERSION: u16 = 0;

/// 의존하는 엔진 커밋. manifest에 남긴다.
pub const ENGINE_REV: &str = "9d1509fe5077d019825f833eed04b16d342dfda1";
