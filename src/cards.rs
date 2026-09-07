//! 카드 인코딩. POKER GOAT 규칙과 postflop-solver 규칙을 서로 변환한다.
//!
//! POKER GOAT: `card = rank * 4 + suit`, rank 0(2)..12(A), suit 0=s 1=h 2=d 3=c.
//! (`pokergoat-user-web/src/lib/poker/equity-worker.ts`의 규칙과 같다.)
//!
//! postflop-solver: `card = 4 * rank + suit`, rank는 같고 suit 0=c 1=d 2=h 3=s.
//!
//! rank 부분은 동일하고 suit 순서만 뒤집혀 있어서 변환은 `suit -> 3 - suit` 하나면 된다.
//! 양방향이 같은 식이라 함수 하나가 자기 자신의 역함수다.

use anyhow::{bail, Result};

/// 우리 규칙의 suit 문자 (인덱스 = suit id)
pub const SUIT_CHARS: [char; 4] = ['s', 'h', 'd', 'c'];
/// rank 문자 (인덱스 = rank id)
pub const RANK_CHARS: [char; 13] = [
    '2', '3', '4', '5', '6', '7', '8', '9', 'T', 'J', 'Q', 'K', 'A',
];

/// 콤보 총 개수 C(52, 2)
pub const NUM_COMBOS: usize = 1326;

/// postflop-solver 카드 id -> 우리 카드 id (역방향도 같은 함수)
#[inline]
pub const fn swap_encoding(card: u8) -> u8 {
    (card >> 2) * 4 + (3 - (card & 3))
}

/// postflop-solver -> POKER GOAT
#[inline]
pub const fn from_lib(card: u8) -> u8 {
    swap_encoding(card)
}

/// POKER GOAT -> postflop-solver
#[inline]
pub const fn to_lib(card: u8) -> u8 {
    swap_encoding(card)
}

#[inline]
pub const fn rank_of(card: u8) -> u8 {
    card >> 2
}

#[inline]
pub const fn suit_of(card: u8) -> u8 {
    card & 3
}

/// 우리 규칙 카드 id를 "As" 같은 문자열로
pub fn card_to_string(card: u8) -> Result<String> {
    if card >= 52 {
        bail!("카드 id 범위 초과: {card}");
    }
    Ok(format!(
        "{}{}",
        RANK_CHARS[rank_of(card) as usize],
        SUIT_CHARS[suit_of(card) as usize]
    ))
}

/// "As" / "as" / "AS" -> 우리 규칙 카드 id
pub fn card_from_str(s: &str) -> Result<u8> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() != 2 {
        bail!("카드 표기는 두 글자여야 한다: {s}");
    }
    let rank_char = chars[0].to_ascii_uppercase();
    let suit_char = chars[1].to_ascii_lowercase();
    let rank = RANK_CHARS
        .iter()
        .position(|&c| c == rank_char)
        .ok_or_else(|| anyhow::anyhow!("알 수 없는 rank: {s}"))?;
    let suit = SUIT_CHARS
        .iter()
        .position(|&c| c == suit_char)
        .ok_or_else(|| anyhow::anyhow!("알 수 없는 suit: {s}"))?;
    Ok(rank as u8 * 4 + suit as u8)
}

/// "As7d2c" -> [48, 22, 3]
pub fn board_from_str(s: &str) -> Result<Vec<u8>> {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.len() % 2 != 0 {
        bail!("보드 문자열 길이가 짝수가 아니다: {s}");
    }
    let chars: Vec<char> = cleaned.chars().collect();
    chars
        .chunks(2)
        .map(|pair| card_from_str(&pair.iter().collect::<String>()))
        .collect()
}

pub fn board_to_string(board: &[u8]) -> Result<String> {
    let mut out = String::new();
    for &card in board {
        out.push_str(&card_to_string(card)?);
    }
    Ok(out)
}

/// 콤보 인덱스. 우리 카드 id 두 장을 0..1325 하나로 접는다.
///
/// `lo < hi`로 정렬한 뒤 `hi * (hi - 1) / 2 + lo`. 2s2h => 0, ..., AcAd => 1325.
/// postflop-solver 내부 인덱싱과 다른 우리 규칙이며 blob에는 항상 이 값을 쓴다.
pub fn combo_index(card1: u8, card2: u8) -> Result<u16> {
    if card1 >= 52 || card2 >= 52 {
        bail!("카드 id 범위 초과: {card1}, {card2}");
    }
    if card1 == card2 {
        bail!("같은 카드 두 장: {card1}");
    }
    let (lo, hi) = if card1 < card2 {
        (card1 as u16, card2 as u16)
    } else {
        (card2 as u16, card1 as u16)
    };
    Ok(hi * (hi - 1) / 2 + lo)
}

/// 콤보 인덱스 -> 카드 두 장 (lo, hi)
pub fn combo_from_index(index: u16) -> Result<(u8, u8)> {
    if index as usize >= NUM_COMBOS {
        bail!("콤보 인덱스 범위 초과: {index}");
    }
    let mut hi = 1u16;
    while hi * (hi + 1) / 2 <= index {
        hi += 1;
    }
    let lo = index - hi * (hi - 1) / 2;
    Ok((lo as u8, hi as u8))
}

/// 카드 한 장을 문자열로 (postflop-solver 규칙 입력)
pub fn lib_card_to_string(lib_card: u8) -> Result<String> {
    card_to_string(from_lib(lib_card))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_round_trip() {
        for card in 0u8..52 {
            assert_eq!(from_lib(to_lib(card)), card);
            assert_eq!(to_lib(from_lib(card)), card);
        }
    }

    #[test]
    fn encoding_matches_library_convention() {
        // 우리 As = rank 12, suit 0 => 48. 라이브러리 As = 4*12 + 3 = 51.
        assert_eq!(card_from_str("As").unwrap(), 48);
        assert_eq!(to_lib(48), 51);
        // 우리 2c = rank 0, suit 3 => 3. 라이브러리 2c = 0.
        assert_eq!(card_from_str("2c").unwrap(), 3);
        assert_eq!(to_lib(3), 0);
        // 7d: rank 5, suit 2 => 22. 라이브러리 = 4*5 + 1 = 21.
        assert_eq!(card_from_str("7d").unwrap(), 22);
        assert_eq!(to_lib(22), 21);
    }

    #[test]
    fn card_string_round_trip() {
        for card in 0u8..52 {
            let s = card_to_string(card).unwrap();
            assert_eq!(card_from_str(&s).unwrap(), card);
        }
    }

    #[test]
    fn board_parsing() {
        assert_eq!(board_from_str("As7d2c").unwrap(), vec![48, 22, 3]);
        assert_eq!(board_to_string(&[48, 22, 3]).unwrap(), "As7d2c");
        assert!(board_from_str("As7d2").is_err());
    }

    #[test]
    fn combo_index_round_trip() {
        let mut seen = vec![false; NUM_COMBOS];
        for lo in 0u8..52 {
            for hi in (lo + 1)..52 {
                let idx = combo_index(lo, hi).unwrap();
                assert_eq!(combo_index(hi, lo).unwrap(), idx);
                assert!(!seen[idx as usize], "콤보 인덱스 중복: {idx}");
                seen[idx as usize] = true;
                assert_eq!(combo_from_index(idx).unwrap(), (lo, hi));
            }
        }
        assert!(seen.into_iter().all(|v| v));
    }

    #[test]
    fn combo_index_bounds() {
        assert_eq!(combo_index(0, 1).unwrap(), 0);
        assert_eq!(combo_index(50, 51).unwrap(), 1325);
        assert!(combo_index(3, 3).is_err());
    }
}
