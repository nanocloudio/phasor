//! Allocation-free seed evaluator shared by Phasor shipping and fixture fmods.
//!
//! This is intentionally not described as an ECMAScript implementation. It
//! accepts a narrow set of decimal integer additive expressions whose syntax is
//! valid JavaScript, spends explicit fuel for every consumed source byte, and
//! establishes the interface later parser and bytecode cores will preserve.

/// Stable failure classes exposed by the seed evaluator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvalError {
    Empty,
    ExpectedNumber,
    InvalidToken,
    TrailingInput,
    BudgetExceeded,
    NumericOverflow,
    OutputTooSmall,
    SourceTooLarge,
}

impl EvalError {
    /// Stable wire token used by the Fluxor wrapper.
    pub const fn token(self) -> &'static [u8] {
        match self {
            Self::Empty => b"empty",
            Self::ExpectedNumber => b"expected-number",
            Self::InvalidToken => b"invalid-token",
            Self::TrailingInput => b"trailing-input",
            Self::BudgetExceeded => b"budget",
            Self::NumericOverflow => b"numeric-overflow",
            Self::OutputTooSmall => b"output-too-small",
            Self::SourceTooLarge => b"source-too-large",
        }
    }
}

struct Parser<'a> {
    source: &'a [u8],
    cursor: usize,
    fuel: u32,
}

impl<'a> Parser<'a> {
    const fn new(source: &'a [u8], fuel: u32) -> Self {
        Self {
            source,
            cursor: 0,
            fuel,
        }
    }

    fn spend(&mut self) -> Result<(), EvalError> {
        self.fuel = self.fuel.checked_sub(1).ok_or(EvalError::BudgetExceeded)?;
        Ok(())
    }

    fn peek(&self) -> Option<u8> {
        self.source.get(self.cursor).copied()
    }

    fn consume(&mut self) -> Result<u8, EvalError> {
        let byte = self.peek().ok_or(EvalError::TrailingInput)?;
        self.spend()?;
        self.cursor += 1;
        Ok(byte)
    }

    fn skip_ascii_whitespace(&mut self) -> Result<(), EvalError> {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            let _ = self.consume()?;
        }
        Ok(())
    }

    fn parse_integer(&mut self) -> Result<i32, EvalError> {
        self.skip_ascii_whitespace()?;

        let negative = match self.peek() {
            Some(b'+') => {
                let _ = self.consume()?;
                self.skip_ascii_whitespace()?;
                false
            }
            Some(b'-') => {
                let _ = self.consume()?;
                self.skip_ascii_whitespace()?;
                true
            }
            _ => false,
        };

        let mut magnitude = 0u32;
        let mut digits = 0usize;
        while let Some(byte @ b'0'..=b'9') = self.peek() {
            let _ = self.consume()?;
            magnitude = magnitude
                .checked_mul(10)
                .and_then(|value| value.checked_add(u32::from(byte - b'0')))
                .ok_or(EvalError::NumericOverflow)?;
            digits += 1;
        }
        if digits == 0 {
            return Err(EvalError::ExpectedNumber);
        }

        if negative {
            if magnitude == 2_147_483_648 {
                Ok(i32::MIN)
            } else {
                let positive = i32::try_from(magnitude).map_err(|_| EvalError::NumericOverflow)?;
                Ok(-positive)
            }
        } else {
            i32::try_from(magnitude).map_err(|_| EvalError::NumericOverflow)
        }
    }

    fn parse(mut self) -> Result<i32, EvalError> {
        self.skip_ascii_whitespace()?;
        if self.peek().is_none() {
            return Err(EvalError::Empty);
        }

        let mut value = self.parse_integer()?;
        loop {
            self.skip_ascii_whitespace()?;
            let Some(operator) = self.peek() else {
                return Ok(value);
            };
            if operator != b'+' && operator != b'-' {
                return if operator.is_ascii() {
                    Err(EvalError::InvalidToken)
                } else {
                    Err(EvalError::TrailingInput)
                };
            }
            let _ = self.consume()?;
            let rhs = self.parse_integer()?;
            value = if operator == b'+' {
                value.checked_add(rhs)
            } else {
                value.checked_sub(rhs)
            }
            .ok_or(EvalError::NumericOverflow)?;
        }
    }
}

fn write_i32(value: i32, output: &mut [u8]) -> Result<usize, EvalError> {
    let negative = value.is_negative();
    let mut magnitude = value.unsigned_abs();
    let mut reversed = [0u8; 10];
    let mut digits = 0usize;

    loop {
        reversed[digits] = b'0' + u8::try_from(magnitude % 10).unwrap_or(0);
        digits += 1;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }

    let required = digits + usize::from(negative);
    if output.len() < required {
        return Err(EvalError::OutputTooSmall);
    }

    let mut cursor = 0usize;
    if negative {
        output[0] = b'-';
        cursor = 1;
    }
    while digits > 0 {
        digits -= 1;
        output[cursor] = reversed[digits];
        cursor += 1;
    }
    Ok(cursor)
}

/// Evaluate one seed expression and encode its decimal result into `output`.
pub fn evaluate(source: &[u8], fuel: u32, output: &mut [u8]) -> Result<usize, EvalError> {
    let value = Parser::new(source, fuel).parse()?;
    write_i32(value, output)
}
