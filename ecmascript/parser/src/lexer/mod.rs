//! ECMAScript lexer.
//!
//! In future, this might use string directly.

#![allow(unused_mut)]
#![allow(unused_variables)]
pub use self::input::Input;
pub(crate) use self::state::{TokenContext, TokenContexts};
use self::{state::State, util::*};
use crate::{
    error::{Error, SyntaxError},
    token::*,
    Context, Session, Syntax,
};
use ast::Str;
use std::char;
use swc_atoms::JsWord;
use swc_common::{BytePos, Span};

pub mod input;
mod jsx;
mod number;
mod state;
#[cfg(test)]
mod tests;
pub mod util;

pub(crate) type LexResult<T> = Result<T, Error>;

#[derive(Clone)]
pub(crate) struct Lexer<'a, I: Input> {
    session: Session<'a>,
    pub ctx: Context,
    input: I,
    state: State,
    pub syntax: Syntax,
}

impl<'a, I: Input> Lexer<'a, I> {
    pub fn new(session: Session<'a>, syntax: Syntax, input: I) -> Self {
        Lexer {
            session,
            input,
            state: State::new(syntax),
            ctx: Default::default(),
            syntax,
        }
    }

    /// babel: `getTokenFromCode`
    fn read_token(&mut self) -> LexResult<Option<Token>> {
        let c = match self.input.cur() {
            Some(c) => c,
            None => return Ok(None),
        };
        let start = self.cur_pos();

        let token = match c {
            '#' => return self.read_token_number_sign(),
            // Identifier or keyword. '\uXXXX' sequences are allowed in
            // identifiers, so '\' also dispatches to that.
            c if c == '\\' || c.is_ident_start() => return self.read_ident_or_keyword().map(Some),

            //
            '.' => {
                // Check for eof
                let next = match self.input.peek() {
                    Some(next) => next,
                    None => {
                        self.input.bump();
                        return Ok(Some(tok!('.')));
                    }
                };
                if '0' <= next && next <= '9' {
                    return self.read_number(true).map(Token::Num).map(Some);
                }

                self.input.bump(); // 1st `.`

                if next == '.' && self.input.peek() == Some('.') {
                    self.input.bump(); // 2nd `.`
                    self.input.bump(); // 3rd `.`

                    return Ok(Some(tok!("...")));
                }

                return Ok(Some(tok!('.')));
            }

            '(' | ')' | ';' | ',' | '[' | ']' | '{' | '}' | '@' | '?' => {
                // These tokens are emitted directly.
                self.input.bump();
                return Ok(Some(match c {
                    '(' => LParen,
                    ')' => RParen,
                    ';' => Semi,
                    ',' => Comma,
                    '[' => LBracket,
                    ']' => RBracket,
                    '{' => LBrace,
                    '}' => RBrace,
                    '@' => At,
                    '?' => QuestionMark,
                    _ => unreachable!(),
                }));
            }

            '`' => {
                self.bump();
                return Ok(Some(tok!('`')));
            }

            ':' => {
                self.input.bump();

                if self.syntax.fn_bind() && self.input.cur() == Some(':') {
                    self.input.bump();
                    return Ok(Some(tok!("::")));
                }

                return Ok(Some(tok!(':')));
            }

            '0' => {
                let next = self.input.peek();

                let radix = match next {
                    Some('x') | Some('X') => 16,
                    Some('o') | Some('O') => 8,
                    Some('b') | Some('B') => 2,
                    _ => return self.read_number(false).map(Num).map(Some),
                };

                return self.read_radix_number(radix).map(Num).map(Some);
            }
            '1'...'9' => return self.read_number(false).map(Num).map(Some),

            '"' | '\'' => return self.read_str_lit().map(Some),

            '/' => return self.read_slash(),

            c @ '%' | c @ '*' => {
                let is_mul = c == '*';
                self.input.bump();
                let mut token = if is_mul { BinOp(Mul) } else { BinOp(Mod) };

                // check for **
                if is_mul {
                    if self.input.cur() == Some('*') {
                        self.input.bump();
                        token = BinOp(Exp)
                    }
                }

                if self.input.cur() == Some('=') {
                    self.input.bump();
                    token = match token {
                        BinOp(Mul) => AssignOp(MulAssign),
                        BinOp(Mod) => AssignOp(ModAssign),
                        BinOp(Exp) => AssignOp(ExpAssign),
                        _ => unreachable!(),
                    }
                }

                token
            }

            // Logical operators
            c @ '|' | c @ '&' => {
                self.input.bump();
                let token = if c == '&' { BitAnd } else { BitOr };

                // '|=', '&='
                if self.input.cur() == Some('=') {
                    self.input.bump();
                    return Ok(Some(AssignOp(match token {
                        BitAnd => BitAndAssign,
                        BitOr => BitOrAssign,
                        _ => unreachable!(),
                    })));
                }

                // '||', '&&'
                if self.input.cur() == Some(c) {
                    self.input.bump();
                    return Ok(Some(BinOp(match token {
                        BitAnd => LogicalAnd,
                        BitOr => LogicalOr,
                        _ => unreachable!(),
                    })));
                }

                BinOp(token)
            }
            '^' => {
                // Bitwise xor
                self.input.bump();
                if self.input.cur() == Some('=') {
                    self.input.bump();
                    AssignOp(BitXorAssign)
                } else {
                    BinOp(BitXor)
                }
            }

            '+' | '-' => {
                self.input.bump();

                // '++', '--'
                if self.input.cur() == Some(c) {
                    self.input.bump();

                    // Handle -->
                    if self.state.had_line_break && c == '-' && self.eat('>') {
                        if self.ctx.module {
                            self.error(start, SyntaxError::LegacyCommentInModule)?
                        }
                        self.skip_line_comment(0);
                        self.skip_space()?;
                        return self.read_token();
                    }

                    if c == '+' {
                        PlusPlus
                    } else {
                        MinusMinus
                    }
                } else if self.input.cur() == Some('=') {
                    self.input.bump();
                    AssignOp(if c == '+' { AddAssign } else { SubAssign })
                } else {
                    BinOp(if c == '+' { Add } else { Sub })
                }
            }

            '<' | '>' => return self.read_token_lt_gt(),

            '!' | '=' => {
                self.input.bump();

                if self.input.cur() == Some('=') {
                    // "=="
                    self.input.bump();

                    if self.input.cur() == Some('=') {
                        self.input.bump();
                        if c == '!' {
                            BinOp(NotEqEq)
                        } else {
                            BinOp(EqEqEq)
                        }
                    } else {
                        if c == '!' {
                            BinOp(NotEq)
                        } else {
                            BinOp(EqEq)
                        }
                    }
                } else if c == '=' && self.input.cur() == Some('>') {
                    // "=>"
                    self.input.bump();

                    Arrow
                } else {
                    if c == '!' {
                        Bang
                    } else {
                        AssignOp(Assign)
                    }
                }
            }
            '~' => {
                self.input.bump();
                tok!('~')
            }

            // unexpected character
            c => self.error_span(pos_span(start), SyntaxError::UnexpectedChar { c })?,
        };

        Ok(Some(token))
    }

    /// `#`
    fn read_token_number_sign(&mut self) -> LexResult<Option<Token>> {
        assert!(self.cur().is_some());

        let start = self.input.cur_pos();

        if self.input.is_at_start() && self.read_token_interpreter()? {
            return Ok(None);
        }

        if self.syntax.class_private_props() || self.syntax.class_private_methods() {
            self.input.bump(); // '#'
            return Ok(Some(Token::Hash));
        }

        self.error(start, SyntaxError::Hash)?
    }

    fn read_token_interpreter(&mut self) -> LexResult<bool> {
        if !self.input.is_at_start() {
            return Ok(false);
        }

        let start = self.input.cur_pos();
        self.input.bump();
        let c = self.input.cur();
        if c == Some('!') {
            loop {
                while let Some(c) = self.input.cur() {
                    if c != '\n' && c != '\r' && c != '\u{8232}' && c != '\u{8233}' {
                        self.input.bump();
                        continue;
                    }
                }
            }
        } else {
            self.input.reset_to(start);
            return Ok(false);
        }
    }

    /// Read an escaped charater for string literal.
    ///
    /// In template literal, we should preserve raw string.
    fn read_escaped_char(&mut self, mut raw: &mut Raw) -> LexResult<Option<char>> {
        debug_assert_eq!(self.cur(), Some('\\'));
        let start = self.cur_pos();
        self.bump(); // '\'

        let in_template = raw.0.is_some();

        let c = match self.cur() {
            Some(c) => c,
            None => self.error_span(pos_span(start), SyntaxError::InvalidStrEscape)?,
        };

        macro_rules! push_c_and_ret {
            ($c:expr) => {{
                raw.push(c);
                $c
            }};
        }

        let c = match c {
            '\\' => push_c_and_ret!('\\'),
            'n' => push_c_and_ret!('\n'),
            'r' => push_c_and_ret!('\r'),
            't' => push_c_and_ret!('\t'),
            'b' => push_c_and_ret!('\u{0008}'),
            'v' => push_c_and_ret!('\u{000b}'),
            'f' => push_c_and_ret!('\u{000c}'),
            '\r' => {
                raw.push_str("\\r");
                self.bump(); // remove '\r'

                if self.cur() == Some('\n') {
                    raw.push_str("\\n");
                    self.bump();
                }
                return Ok(None);
            }
            '\n' | '\u{2028}' | '\u{2029}' => {
                match c {
                    '\n' => raw.push_str("\\n"),
                    '\u{2028}' => raw.push_str("\\u{2028}"),
                    '\u{2029}' => raw.push_str("\\u{2029}"),
                    _ => unreachable!(),
                }
                self.bump();
                return Ok(None);
            }

            // read hexadecimal escape sequences
            'x' => {
                raw.push_str("0x");
                self.bump(); // 'x'
                return self.read_hex_char(start, 2, raw).map(Some);
            }

            // read unicode escape sequences
            'u' => {
                return self.read_unicode_escape(start, raw).map(Some);
            }
            // octal escape sequences
            '0'...'7' => {
                self.bump();
                let first_c = if c == '0' {
                    match self.cur() {
                        Some(next) if next.is_digit(8) => c,
                        // \0 is not an octal literal nor decimal literal.
                        _ => return Ok(Some('\u{0000}')),
                    }
                } else {
                    c
                };

                // TODO: Show template instead of strict mode
                if in_template {
                    self.error(start, SyntaxError::LegacyOctal)?
                }

                if self.ctx.strict {
                    self.error(start, SyntaxError::LegacyOctal)?
                }

                let mut value: u8 = first_c.to_digit(8).unwrap() as u8;
                macro_rules! one {
                    ($check:expr) => {{
                        match self.cur().and_then(|c| c.to_digit(8)) {
                            Some(v) => {
                                value = if $check {
                                    let new_val = value
                                        .checked_mul(8)
                                        .and_then(|value| value.checked_add(v as u8));
                                    match new_val {
                                        Some(val) => val,
                                        None => return Ok(Some(value as char)),
                                    }
                                } else {
                                    value * 8 + v as u8
                                };
                                self.bump();
                            }
                            _ => return Ok(Some(value as char)),
                        }
                    }};
                }
                one!(false);
                one!(true);

                return Ok(Some(value as char));
            }
            _ => c,
        };
        self.input.bump();

        Ok(Some(c))
    }
}

impl<'a, I: Input> Lexer<'a, I> {
    fn read_slash(&mut self) -> LexResult<Option<Token>> {
        debug_assert_eq!(self.cur(), Some('/'));
        let start = self.cur_pos();

        // Regex
        if self.state.is_expr_allowed {
            return self.read_regexp().map(Some);
        }

        // Divide operator
        self.bump();

        Ok(Some(if self.eat('=') { tok!("/=") } else { tok!('/') }))
    }

    fn read_token_lt_gt(&mut self) -> LexResult<Option<Token>> {
        assert!(self.cur() == Some('<') || self.cur() == Some('>'));

        let start = self.cur_pos();
        let c = self.cur().unwrap();
        self.bump();

        // XML style comment. `<!--`
        if c == '<' && self.is('!') && self.peek() == Some('-') && self.peek_ahead() == Some('-') {
            self.skip_line_comment(3);
            self.skip_space()?;
            if self.ctx.module {
                self.error(start, SyntaxError::LegacyCommentInModule)?;
            }
            return self.read_token();
        }

        let mut op = if c == '<' { Lt } else { Gt };

        // '<<', '>>'
        if self.cur() == Some(c) {
            self.bump();
            op = if c == '<' { LShift } else { RShift };

            //'>>>'
            if c == '>' && self.cur() == Some(c) {
                self.bump();
                op = ZeroFillRShift;
            }
        }

        let token = if self.eat('=') {
            match op {
                Lt => BinOp(LtEq),
                Gt => BinOp(GtEq),
                LShift => AssignOp(LShiftAssign),
                RShift => AssignOp(RShiftAssign),
                ZeroFillRShift => AssignOp(ZeroFillRShiftAssign),
                _ => unreachable!(),
            }
        } else {
            BinOp(op)
        };

        Ok(Some(token))
    }

    /// See https://tc39.github.io/ecma262/#sec-names-and-keywords
    fn read_ident_or_keyword(&mut self) -> LexResult<Token> {
        assert!(self.cur().is_some());
        let start = self.cur_pos();

        let (word, has_escape) = self.read_word_as_str()?;

        // Note: ctx is store in lexer because of this error.
        // 'await' and 'yield' may have semantic of reserved word, which means lexer
        // should know context or parser should handle this error. Our approach to this
        // problem is former one.
        if has_escape && self.ctx.is_reserved_word(&word) {
            self.error(
                start,
                SyntaxError::EscapeInReservedWord { word: word.into() },
            )?
        } else {
            Ok(Word(word.into()))
        }
    }

    fn may_read_word_as_str(&mut self) -> LexResult<(Option<(JsWord, bool)>)> {
        match self.cur() {
            Some(c) if c.is_ident_start() => self.read_word_as_str().map(Some),
            _ => Ok(None),
        }
    }

    /// returns (word, has_escape)
    fn read_word_as_str(&mut self) -> LexResult<(JsWord, bool)> {
        assert!(self.cur().is_some());

        let mut has_escape = false;
        let mut word = String::new();
        let mut first = true;

        while let Some(c) = {
            // Optimization
            {
                let s = self.input.uncons_while(|c| c.is_ident_part());
                if s.len() != 0 {
                    first = false;
                }
                word.push_str(s)
            };

            self.cur()
        } {
            let start = self.cur_pos();

            match c {
                c if c.is_ident_part() => {
                    self.bump();
                    word.push(c);
                }
                // unicode escape
                '\\' => {
                    self.bump();
                    if !self.is('u') {
                        self.error_span(pos_span(start), SyntaxError::ExpectedUnicodeEscape)?
                    }
                    let c = self.read_unicode_escape(start, &mut Raw(None))?;
                    let valid = if first {
                        c.is_ident_start()
                    } else {
                        c.is_ident_part()
                    };

                    if !valid {
                        self.error(start, SyntaxError::InvalidIdentChar)?
                    }
                    word.push(c);
                }

                _ => {
                    break;
                }
            }
            first = false;
        }
        Ok((word.into(), has_escape))
    }

    fn read_unicode_escape(&mut self, start: BytePos, raw: &mut Raw) -> LexResult<char> {
        debug_assert_eq!(self.cur(), Some('u'));
        self.bump();

        raw.push_str("u");

        if self.eat('{') {
            raw.push('{');
            let cp_start = self.cur_pos();
            let c = self.read_code_point(raw)?;

            if !self.eat('}') {
                self.error(start, SyntaxError::InvalidUnicodeEscape)?
            }
            raw.push('}');

            Ok(c)
        } else {
            self.read_hex_char(start, 4, raw)
        }
    }

    fn read_hex_char(&mut self, start: BytePos, count: u8, mut raw: &mut Raw) -> LexResult<char> {
        debug_assert!(count == 2 || count == 4);

        let pos = self.cur_pos();
        match self.read_int(16, count, raw)? {
            Some(val) => match char::from_u32(val) {
                Some(c) => Ok(c),
                None => self.error(start, SyntaxError::NonUtf8Char { val })?,
            },
            None => self.error(start, SyntaxError::ExpectedHexChars { count })?,
        }
    }

    /// Read `CodePoint`.
    fn read_code_point(&mut self, mut raw: &mut Raw) -> LexResult<char> {
        let start = self.cur_pos();
        let val = self.read_int(16, 0, raw)?;
        match val {
            Some(val) if 0x10FFFF >= val => match char::from_u32(val) {
                Some(c) => Ok(c),
                None => self.error(start, SyntaxError::InvalidCodePoint)?,
            },
            _ => self.error(start, SyntaxError::InvalidCodePoint)?,
        }
    }

    /// See https://tc39.github.io/ecma262/#sec-literals-string-literals
    fn read_str_lit(&mut self) -> LexResult<Token> {
        assert!(self.cur() == Some('\'') || self.cur() == Some('"'));
        let start = self.cur_pos();
        let quote = self.cur().unwrap();
        self.bump(); // '"'

        let mut out = String::new();
        let mut has_escape = false;

        while let Some(c) = {
            // Optimization
            {
                let s = self
                    .input
                    .uncons_while(|c| c != quote && c != '\\' && !c.is_line_break());
                out.push_str(s);
            }
            self.cur()
        } {
            match c {
                c if c == quote => {
                    self.bump();
                    return Ok(Token::Str {
                        value: out.into(),
                        has_escape,
                    });
                }
                '\\' => {
                    out.extend(self.read_escaped_char(&mut Raw(None))?);
                    has_escape = true
                }
                c if c.is_line_break() => self.error(start, SyntaxError::UnterminatedStrLit)?,
                _ => {
                    out.push(c);
                    self.bump();
                }
            }
        }

        self.error(start, SyntaxError::UnterminatedStrLit)?
    }

    /// Expects current char to be '/'
    fn read_regexp(&mut self) -> LexResult<Token> {
        debug_assert_eq!(self.cur(), Some('/'));
        let start = self.cur_pos();
        self.bump();

        let (mut escaped, mut in_class) = (false, false);
        // TODO: Optimize (chunk, cow)
        let mut content = String::new();
        let content_start = self.cur_pos();

        while let Some(c) = self.cur() {
            // This is ported from babel.
            // Seems like regexp literal cannot contain linebreak.
            if c.is_line_break() {
                self.error(start, SyntaxError::UnterminatedRegxp)?;
            }

            if escaped {
                escaped = false;
            } else {
                match c {
                    '[' => in_class = true,
                    ']' if in_class => in_class = false,
                    // Termniates content part of regex literal
                    '/' if !in_class => break,
                    _ => {}
                }
                escaped = c == '\\';
            }
            self.bump();
            content.push(c);
        }
        let content_span = Span::new(content_start, self.cur_pos(), Default::default());

        // input is terminated without following `/`
        if !self.is('/') {
            self.error(start, SyntaxError::UnterminatedRegxp)?;
        }

        self.bump(); // '/'

        // Spec says "It is a Syntax Error if IdentifierPart contains a Unicode escape
        // sequence." TODO: check for escape

        // Need to use `read_word` because '\uXXXX' sequences are allowed
        // here (don't ask).
        let flags_start = self.cur_pos();
        let flags = self.may_read_word_as_str()?.map(|(value, has_escape)| Str {
            span: Span::new(flags_start, self.cur_pos(), Default::default()),
            value,
            has_escape,
        });

        Ok(Regex(
            Str {
                span: content_span,
                value: content.into(),
                // TODO
                has_escape: false,
            },
            flags,
        ))
    }

    fn read_tmpl_token(&mut self, start_of_tpl: BytePos) -> LexResult<Token> {
        let start = self.cur_pos();

        // TODO: Optimize
        let mut has_escape = false;
        let mut cooked = String::new();
        let mut raw = String::new();

        while let Some(c) = self.cur() {
            if c == '`' || (c == '$' && self.peek() == Some('{')) {
                if start == self.cur_pos() && self.state.last_was_tpl_element() {
                    if c == '$' {
                        self.bump();
                        self.bump();
                        return Ok(tok!("${"));
                    } else {
                        self.bump();
                        return Ok(tok!('`'));
                    }
                }

                // TODO: Handle error
                return Ok(Template {
                    cooked: cooked.into(),
                    raw: raw.into(),
                    has_escape,
                });
            }

            if c == '\\' {
                has_escape = true;
                raw.push('\\');
                let mut wrapped = Raw(Some(raw));
                let ch = self.read_escaped_char(&mut wrapped)?;
                raw = wrapped.0.unwrap();
                cooked.extend(ch);
            } else if c.is_line_break() {
                self.state.had_line_break = true;
                let c = if c == '\r' && self.peek() == Some('\n') {
                    raw.push_str("\\r\\n");
                    self.bump(); // '\r'
                    '\n'
                } else {
                    match c {
                        '\n' => raw.push_str("\\n"),
                        '\r' => raw.push_str("\\r"),
                        '\u{2028}' => raw.push_str("\\u{2028}"),
                        '\u{2029}' => raw.push_str("\\u{2029}"),
                        _ => unreachable!(),
                    }
                    c
                };
                self.bump();
                cooked.push(c);
            } else {
                self.bump();
                cooked.push(c);
                raw.push(c);
            }
        }

        self.error(start_of_tpl, SyntaxError::UnterminatedTpl)?
    }

    pub fn had_line_break_before_last(&self) -> bool {
        self.state.had_line_break
    }

    pub fn set_expr_allowed(&mut self, allow: bool) {
        self.state.is_expr_allowed = allow;
    }
}

fn pos_span(p: BytePos) -> Span {
    Span::new(p, p, Default::default())
}
