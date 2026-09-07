//! 부록 A 레인지 표기를 postflop-solver의 `Range` 파서 입력으로 바꾼다.
//!
//! 두 문법의 차이는 세 가지뿐이다.
//!
//! 1. 구분자. 우리는 쉼표와 공백을 모두 허용하고, 라이브러리는 쉼표만 나눈다.
//! 2. 덮어쓰기 방향. 우리 규칙은 뒤 토큰이 앞을 덮어쓰고, 라이브러리는 앞 토큰이 이긴다
//!    (`FromStr`이 토큰을 역순으로 적용한다). 그래서 토큰 순서를 뒤집어서 넘긴다.
//! 3. 대소문자. 우리 쪽 입력은 `t9s`, `ahkh` 같은 표기도 받아준다.
//!
//! `22+`, `A2s+`, `T9s-65s`, `AK`(수딧과 오프수트 둘 다), `AhKh`, `KQo:0.5`는
//! 라이브러리가 그대로 이해하므로 의미 변환은 필요 없다.

use anyhow::{bail, Context, Result};
use postflop_solver::Range;

/// 우리 표기 -> 라이브러리 표기 문자열
pub fn to_library_notation(input: &str) -> Result<String> {
    let tokens = split_tokens(input)?;
    if tokens.is_empty() {
        bail!("레인지가 비어 있다");
    }
    // 뒤 토큰이 앞을 덮어쓰는 우리 규칙 -> 앞 토큰이 이기는 라이브러리 규칙
    let reversed: Vec<String> = tokens.into_iter().rev().collect();
    Ok(reversed.join(","))
}

/// 우리 표기를 파싱해 라이브러리 `Range`로
pub fn parse_range(input: &str) -> Result<Range> {
    let notation = to_library_notation(input)?;
    notation
        .parse::<Range>()
        .map_err(|e| anyhow::anyhow!("레인지 파싱 실패: {e}"))
        .with_context(|| format!("입력 = {input:?} (변환 결과 = {notation:?})"))
}

fn split_tokens(input: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    for raw in input.split([',', ' ', '\t', '\n', '\r']) {
        let token = raw.trim();
        if token.is_empty() {
            continue;
        }
        tokens.push(normalize_token(token)?);
    }
    Ok(tokens)
}

/// rank는 대문자로, suit와 수딧/오프수트 표시는 소문자로 맞춘다.
/// rank 문자와 suit 문자가 겹치지 않아서 한 번에 처리할 수 있다.
fn normalize_token(token: &str) -> Result<String> {
    let mut out = String::with_capacity(token.len());
    for ch in token.chars() {
        let upper = ch.to_ascii_uppercase();
        let mapped = match upper {
            'S' | 'H' | 'D' | 'C' | 'O' => upper.to_ascii_lowercase(),
            other => other,
        };
        match mapped {
            '2'..='9' | 'T' | 'J' | 'Q' | 'K' | 'A' | 's' | 'h' | 'd' | 'c' | 'o' | '+' | '-'
            | ':' | '.' | '0' | '1' => out.push(mapped),
            _ => bail!("레인지 토큰에 쓸 수 없는 문자 {ch:?}: {token}"),
        }
    }
    Ok(out)
}

/// 레인지의 총 콤보 수 (가중치 합). 검증과 리포트용.
pub fn total_weight(range: &Range) -> f32 {
    range.raw_data().iter().sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weight_of(range: &Range, combo: &str) -> f32 {
        // "AhKh" 같은 구체 콤보 하나의 가중치
        let chars: Vec<char> = combo.chars().collect();
        let card1 = crate::cards::to_lib(crate::cards::card_from_str(&format!("{}{}", chars[0], chars[1])).unwrap());
        let card2 = crate::cards::to_lib(crate::cards::card_from_str(&format!("{}{}", chars[2], chars[3])).unwrap());
        range.get_weight_by_cards(card1, card2)
    }

    #[test]
    fn separators_and_case() {
        let a = parse_range("AA, KK").unwrap();
        let b = parse_range("aa kk").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn plus_range() {
        let range = parse_range("22+").unwrap();
        for rank in 0..13u8 {
            assert_eq!(range.get_weight_pair(rank), 1.0);
        }
        let range = parse_range("A2s+").unwrap();
        assert_eq!(range.get_weight_suited(12, 0), 1.0);
        assert_eq!(range.get_weight_suited(12, 11), 1.0);
        assert_eq!(range.get_weight_offsuit(12, 11), 0.0);
    }

    #[test]
    fn dash_range_maps_directly() {
        let range = parse_range("T9s-65s").unwrap();
        for (r1, r2) in [(8u8, 7u8), (7, 6), (6, 5), (5, 4), (4, 3)] {
            assert_eq!(range.get_weight_suited(r1, r2), 1.0, "{r1}-{r2}");
        }
        assert_eq!(range.get_weight_suited(9, 8), 0.0);
        assert_eq!(range.get_weight_suited(3, 2), 0.0);
    }

    #[test]
    fn bare_two_ranks_mean_both_suitedness() {
        let range = parse_range("AK").unwrap();
        assert_eq!(range.get_weight_suited(12, 11), 1.0);
        assert_eq!(range.get_weight_offsuit(12, 11), 1.0);
    }

    #[test]
    fn specific_combo_keeps_our_suit_letters() {
        let range = parse_range("AhKh").unwrap();
        assert_eq!(weight_of(&range, "AhKh"), 1.0);
        assert_eq!(weight_of(&range, "AsKs"), 0.0);
        assert_eq!(weight_of(&range, "AcKc"), 0.0);
    }

    #[test]
    fn weight_suffix() {
        let range = parse_range("KQo:0.5").unwrap();
        assert_eq!(range.get_weight_offsuit(11, 10), 0.5);
    }

    #[test]
    fn later_token_overrides_earlier() {
        // 우리 규칙: 뒤 토큰이 이긴다
        let range = parse_range("AA, AA:0.25").unwrap();
        assert_eq!(range.get_weight_pair(12), 0.25);
        let range = parse_range("22+:1, TT:0.5").unwrap();
        assert_eq!(range.get_weight_pair(8), 0.5);
        assert_eq!(range.get_weight_pair(9), 1.0);
    }

    #[test]
    fn empty_input_rejected() {
        assert!(parse_range("   ").is_err());
        assert!(parse_range("").is_err());
    }

    #[test]
    fn junk_rejected() {
        assert!(parse_range("XX").is_err());
        assert!(parse_range("A*").is_err());
    }

    #[test]
    fn combo_count_of_pair_is_six() {
        let range = parse_range("QQ").unwrap();
        assert_eq!(total_weight(&range), 6.0);
    }
}
