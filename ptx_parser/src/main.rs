use gen::derive_parser;
use logos::Logos;
use std::mem;
use std::num::{ParseFloatError, ParseIntError};
use winnow::ascii::{dec_uint, digit1};
use winnow::combinator::*;
use winnow::error::ErrMode;
use winnow::stream::Accumulate;
use winnow::token::any;
use winnow::{
    error::{ContextError, ParserError},
    stream::{Offset, Stream, StreamIsPartial},
    PResult,
};
use winnow::{prelude::*, Stateful};

mod ast;
pub use ast::*;

impl From<RawStCacheOperator> for ast::StCacheOperator {
    fn from(value: RawStCacheOperator) -> Self {
        match value {
            RawStCacheOperator::Wb => ast::StCacheOperator::Writeback,
            RawStCacheOperator::Cg => ast::StCacheOperator::L2Only,
            RawStCacheOperator::Cs => ast::StCacheOperator::Streaming,
            RawStCacheOperator::Wt => ast::StCacheOperator::Writethrough,
        }
    }
}

impl From<RawLdCacheOperator> for ast::LdCacheOperator {
    fn from(value: RawLdCacheOperator) -> Self {
        match value {
            RawLdCacheOperator::Ca => ast::LdCacheOperator::Cached,
            RawLdCacheOperator::Cg => ast::LdCacheOperator::L2Only,
            RawLdCacheOperator::Cs => ast::LdCacheOperator::Streaming,
            RawLdCacheOperator::Lu => ast::LdCacheOperator::LastUse,
            RawLdCacheOperator::Cv => ast::LdCacheOperator::Uncached,
        }
    }
}

impl From<RawLdStQualifier> for ast::LdStQualifier {
    fn from(value: RawLdStQualifier) -> Self {
        match value {
            RawLdStQualifier::Weak => ast::LdStQualifier::Weak,
            RawLdStQualifier::Volatile => ast::LdStQualifier::Volatile,
        }
    }
}

impl From<RawFloatRounding> for ast::RoundingMode {
    fn from(value: RawFloatRounding) -> Self {
        match value {
            RawFloatRounding::Rn => ast::RoundingMode::NearestEven,
            RawFloatRounding::Rz => ast::RoundingMode::Zero,
            RawFloatRounding::Rm => ast::RoundingMode::NegativeInf,
            RawFloatRounding::Rp => ast::RoundingMode::PositiveInf,
        }
    }
}

type PtxParserState = Vec<PtxError>;
type PtxParser<'a, 'input> = Stateful<&'a [Token<'input>], PtxParserState>;

fn ident<'a, 'input>(stream: &mut PtxParser<'a, 'input>) -> PResult<&'input str> {
    any.verify_map(|t| {
        if let Token::Ident(text) = t {
            Some(text)
        } else if let Some(text) = t.opcode_text() {
            Some(text)
        } else {
            None
        }
    })
    .parse_next(stream)
}

fn num<'a, 'input>(stream: &mut PtxParser<'a, 'input>) -> PResult<(&'input str, u32, bool)> {
    any.verify_map(|t| {
        Some(match t {
            Token::Hex(s) => {
                if s.ends_with('U') {
                    (&s[2..s.len() - 1], 16, true)
                } else {
                    (&s[2..], 16, false)
                }
            }
            Token::Decimal(s) => {
                let radix = if s.starts_with('0') { 8 } else { 10 };
                if s.ends_with('U') {
                    (&s[..s.len() - 1], radix, true)
                } else {
                    (s, radix, false)
                }
            }
            _ => return None,
        })
    })
    .parse_next(stream)
}

fn take_error<'a, 'input: 'a, O, E>(
    mut parser: impl Parser<PtxParser<'a, 'input>, Result<O, (O, PtxError)>, E>,
) -> impl Parser<PtxParser<'a, 'input>, O, E> {
    move |input: &mut PtxParser<'a, 'input>| {
        Ok(match parser.parse_next(input)? {
            Ok(x) => x,
            Err((x, err)) => {
                input.state.push(err);
                x
            }
        })
    }
}

fn int_immediate<'a, 'input>(input: &mut PtxParser<'a, 'input>) -> PResult<ast::ImmediateValue> {
    take_error((opt(Token::Minus), num).map(|(neg, x)| {
        let (num, radix, is_unsigned) = x;
        if neg.is_some() {
            match i64::from_str_radix(num, radix) {
                Ok(x) => Ok(ast::ImmediateValue::S64(-x)),
                Err(err) => Err((ast::ImmediateValue::S64(0), PtxError::from(err))),
            }
        } else if is_unsigned {
            match u64::from_str_radix(num, radix) {
                Ok(x) => Ok(ast::ImmediateValue::U64(x)),
                Err(err) => Err((ast::ImmediateValue::U64(0), PtxError::from(err))),
            }
        } else {
            match i64::from_str_radix(num, radix) {
                Ok(x) => Ok(ast::ImmediateValue::S64(x)),
                Err(_) => match u64::from_str_radix(num, radix) {
                    Ok(x) => Ok(ast::ImmediateValue::U64(x)),
                    Err(err) => Err((ast::ImmediateValue::U64(0), PtxError::from(err))),
                },
            }
        }
    }))
    .parse_next(input)
}

fn f32<'a, 'input>(stream: &mut PtxParser<'a, 'input>) -> PResult<f32> {
    take_error(any.verify_map(|t| match t {
        Token::F32(f) => Some(match u32::from_str_radix(&f[2..], 16) {
            Ok(x) => Ok(f32::from_bits(x)),
            Err(err) => Err((0.0, PtxError::from(err))),
        }),
        _ => None,
    }))
    .parse_next(stream)
}

fn f64<'a, 'input>(stream: &mut PtxParser<'a, 'input>) -> PResult<f64> {
    take_error(any.verify_map(|t| match t {
        Token::F64(f) => Some(match u64::from_str_radix(&f[2..], 16) {
            Ok(x) => Ok(f64::from_bits(x)),
            Err(err) => Err((0.0, PtxError::from(err))),
        }),
        _ => None,
    }))
    .parse_next(stream)
}

fn s32<'a, 'input>(stream: &mut PtxParser<'a, 'input>) -> PResult<i32> {
    take_error((opt(Token::Minus), num).map(|(sign, x)| {
        let (text, radix, _) = x;
        match i32::from_str_radix(text, radix) {
            Ok(x) => Ok(if sign.is_some() { -x } else { x }),
            Err(err) => Err((0, PtxError::from(err))),
        }
    }))
    .parse_next(stream)
}

fn u8<'a, 'input>(stream: &mut PtxParser<'a, 'input>) -> PResult<u8> {
    take_error(num.map(|x| {
        let (text, radix, _) = x;
        match u8::from_str_radix(text, radix) {
            Ok(x) => Ok(x),
            Err(err) => Err((0, PtxError::from(err))),
        }
    }))
    .parse_next(stream)
}

fn u32<'a, 'input>(stream: &mut PtxParser<'a, 'input>) -> PResult<u32> {
    take_error(num.map(|x| {
        let (text, radix, _) = x;
        match u32::from_str_radix(text, radix) {
            Ok(x) => Ok(x),
            Err(err) => Err((0, PtxError::from(err))),
        }
    }))
    .parse_next(stream)
}

fn immediate_value<'a, 'input>(stream: &mut PtxParser<'a, 'input>) -> PResult<ast::ImmediateValue> {
    alt((
        int_immediate,
        f32.map(ast::ImmediateValue::F32),
        f64.map(ast::ImmediateValue::F64),
    ))
    .parse_next(stream)
}

fn module<'a, 'input>(stream: &mut PtxParser<'a, 'input>) -> PResult<ast::Module<'input>> {
    (
        version,
        target,
        opt(address_size),
        repeat_without_none(directive),
    )
        .map(|(version, _, _, directives)| ast::Module {
            version,
            directives,
        })
        .parse_next(stream)
}

fn address_size<'a, 'input>(stream: &mut PtxParser<'a, 'input>) -> PResult<()> {
    (Token::DotAddressSize, u8_literal(64))
        .void()
        .parse_next(stream)
}

fn version<'a, 'input>(stream: &mut PtxParser<'a, 'input>) -> PResult<(u8, u8)> {
    (Token::DotVersion, u8, Token::Dot, u8)
        .map(|(_, major, _, minor)| (major, minor))
        .parse_next(stream)
}

fn target<'a, 'input>(stream: &mut PtxParser<'a, 'input>) -> PResult<(u32, Option<char>)> {
    preceded(Token::DotTarget, ident.and_then(shader_model)).parse_next(stream)
}

fn shader_model<'a>(stream: &mut &str) -> PResult<(u32, Option<char>)> {
    (
        "sm_",
        dec_uint,
        opt(any.verify(|c: &char| c.is_ascii_lowercase())),
        eof,
    )
        .map(|(_, digits, arch_variant, _)| (digits, arch_variant))
        .parse_next(stream)
}

fn directive<'a, 'input>(
    stream: &mut PtxParser<'a, 'input>,
) -> PResult<Option<ast::Directive<'input, ast::ParsedOperand<&'input str>>>> {
    (function.map(|f| {
        let (linking, func) = f;
        Some(ast::Directive::Method(linking, func))
    }))
    .parse_next(stream)
}

fn function<'a, 'input>(
    stream: &mut PtxParser<'a, 'input>,
) -> PResult<(
    ast::LinkingDirective,
    ast::Function<'input, &'input str, ast::Statement<ParsedOperand<&'input str>>>,
)> {
    (
        linking_directives,
        method_declaration,
        repeat(0.., tuning_directive),
        function_body,
    )
        .map(|(linking, func_directive, tuning, body)| {
            (
                linking,
                ast::Function {
                    func_directive,
                    tuning,
                    body,
                },
            )
        })
        .parse_next(stream)
}

fn linking_directives<'a, 'input>(
    stream: &mut PtxParser<'a, 'input>,
) -> PResult<ast::LinkingDirective> {
    dispatch! { any;
        Token::DotExtern => empty.value(ast::LinkingDirective::EXTERN),
        Token::DotVisible => empty.value(ast::LinkingDirective::VISIBLE),
        Token::DotWeak => empty.value(ast::LinkingDirective::WEAK),
        _ => fail
    }
    .parse_next(stream)
}

fn tuning_directive<'a, 'input>(
    stream: &mut PtxParser<'a, 'input>,
) -> PResult<ast::TuningDirective> {
    dispatch! {any;
        Token::DotMaxnreg => u32.map(ast::TuningDirective::MaxNReg),
        Token::DotMaxntid => tuple1to3_u32.map(|(nx, ny, nz)| ast::TuningDirective::MaxNtid(nx, ny, nz)),
        Token::DotReqntid => tuple1to3_u32.map(|(nx, ny, nz)| ast::TuningDirective::ReqNtid(nx, ny, nz)),
        Token::DotMinnctapersm => u32.map(ast::TuningDirective::MinNCtaPerSm),
        _ => fail
    }
    .parse_next(stream)
}

fn method_declaration<'a, 'input>(
    stream: &mut PtxParser<'a, 'input>,
) -> PResult<ast::MethodDeclaration<'input, &'input str>> {
    dispatch! {any;
        Token::DotEntry => (ident, kernel_arguments).map(|(name, input_arguments)| ast::MethodDeclaration{
            return_arguments: Vec::new(), name: ast::MethodName::Kernel(name), input_arguments, shared_mem: None
        }),
        Token::DotFunc => (opt(fn_arguments), ident, fn_arguments).map(|(return_arguments, name,input_arguments)| {
            let return_arguments = return_arguments.unwrap_or_else(|| Vec::new());
            let name = ast::MethodName::Func(name);
            ast::MethodDeclaration{ return_arguments, name, input_arguments, shared_mem: None }
        }),
        _ => fail
    }
    .parse_next(stream)
}

fn fn_arguments<'a, 'input>(
    stream: &mut PtxParser<'a, 'input>,
) -> PResult<Vec<ast::Variable<&'input str>>> {
    delimited(
        Token::LParen,
        separated(0.., fn_input, Token::Comma),
        Token::RParen,
    )
    .parse_next(stream)
}

fn kernel_arguments<'a, 'input>(
    stream: &mut PtxParser<'a, 'input>,
) -> PResult<Vec<ast::Variable<&'input str>>> {
    delimited(
        Token::LParen,
        separated(0.., kernel_input, Token::Comma),
        Token::RParen,
    )
    .parse_next(stream)
}

fn kernel_input<'a, 'input>(
    stream: &mut PtxParser<'a, 'input>,
) -> PResult<ast::Variable<&'input str>> {
    preceded(
        Token::DotParam,
        variable_scalar_or_vector(StateSpace::Param),
    )
    .parse_next(stream)
}

fn fn_input<'a, 'input>(stream: &mut PtxParser<'a, 'input>) -> PResult<ast::Variable<&'input str>> {
    dispatch! { any;
        Token::DotParam => variable_scalar_or_vector(StateSpace::Param),
        Token::DotReg => variable_scalar_or_vector(StateSpace::Reg),
        _ => fail
    }
    .parse_next(stream)
}

fn tuple1to3_u32<'a, 'input>(stream: &mut PtxParser<'a, 'input>) -> PResult<(u32, u32, u32)> {
    struct Tuple3AccumulateU32 {
        index: usize,
        value: (u32, u32, u32),
    }

    impl Accumulate<u32> for Tuple3AccumulateU32 {
        fn initial(_: Option<usize>) -> Self {
            Self {
                index: 0,
                value: (1, 1, 1),
            }
        }

        fn accumulate(&mut self, value: u32) {
            match self.index {
                0 => {
                    self.value = (value, self.value.1, self.value.2);
                    self.index = 1;
                }
                1 => {
                    self.value = (self.value.0, value, self.value.2);
                    self.index = 2;
                }
                2 => {
                    self.value = (self.value.0, self.value.1, value);
                    self.index = 3;
                }
                _ => unreachable!(),
            }
        }
    }

    separated::<_, _, Tuple3AccumulateU32, _, _, _, _>(1..3, u32, Token::Comma)
        .map(|acc| acc.value)
        .parse_next(stream)
}

fn function_body<'a, 'input>(
    stream: &mut PtxParser<'a, 'input>,
) -> PResult<Option<Vec<ast::Statement<ParsedOperandStr<'input>>>>> {
    dispatch! {any;
        Token::LBrace => terminated(repeat_without_none(statement), Token::RBrace).map(Some),
        Token::Semicolon => empty.map(|_| None),
        _ => fail
    }
    .parse_next(stream)
}

fn statement<'a, 'input>(
    stream: &mut PtxParser<'a, 'input>,
) -> PResult<Option<Statement<ParsedOperandStr<'input>>>> {
    alt((
        label.map(Some),
        debug_directive.map(|_| None),
        multi_variable.map(Some),
        predicated_instruction.map(Some),
        pragma.map(|_| None),
        block_statement.map(Some),
    ))
    .parse_next(stream)
}

fn pragma<'a, 'input>(stream: &mut PtxParser<'a, 'input>) -> PResult<()> {
    (Token::DotPragma, Token::String, Token::Semicolon)
        .void()
        .parse_next(stream)
}

fn multi_variable<'a, 'input>(
    stream: &mut PtxParser<'a, 'input>,
) -> PResult<ast::Statement<ParsedOperandStr<'input>>> {
    (
        variable,
        opt(delimited(Token::Lt, u32, Token::Gt)),
        Token::Semicolon,
    )
        .map(|(var, count, _)| ast::Statement::Variable(ast::MultiVariable { var, count }))
        .parse_next(stream)
}

fn variable<'a, 'input>(stream: &mut PtxParser<'a, 'input>) -> PResult<ast::Variable<&'input str>> {
    dispatch! {any;
        Token::DotReg => variable_scalar_or_vector(StateSpace::Reg),
        Token::DotLocal => variable_scalar_or_vector(StateSpace::Local),
        Token::DotParam => variable_scalar_or_vector(StateSpace::Param),
        Token::DotShared => variable_scalar_or_vector(StateSpace::Shared),
        _ => fail
    }
    .parse_next(stream)
}

fn variable_scalar_or_vector<'a, 'input: 'a>(
    state_space: StateSpace,
) -> impl Parser<PtxParser<'a, 'input>, ast::Variable<&'input str>, ContextError> {
    move |stream: &mut PtxParser<'a, 'input>| {
        (opt(align), scalar_vector_type, ident)
            .map(|(align, v_type, name)| ast::Variable {
                align,
                v_type,
                state_space,
                name,
                array_init: Vec::new(),
            })
            .parse_next(stream)
    }
}

fn align<'a, 'input>(stream: &mut PtxParser<'a, 'input>) -> PResult<u32> {
    preceded(Token::DotAlign, u32).parse_next(stream)
}

fn scalar_vector_type<'a, 'input>(stream: &mut PtxParser<'a, 'input>) -> PResult<Type> {
    (
        opt(alt((
            Token::DotV2.value(VectorPrefix::V2),
            Token::DotV4.value(VectorPrefix::V4),
        ))),
        scalar_type,
    )
        .map(|(prefix, scalar)| ast::Type::maybe_vector(prefix, scalar))
        .parse_next(stream)
}

fn scalar_type<'a, 'input>(stream: &mut PtxParser<'a, 'input>) -> PResult<ScalarType> {
    any.verify_map(|t| {
        Some(match t {
            Token::DotS8 => ScalarType::S8,
            Token::DotS16 => ScalarType::S16,
            Token::DotS16x2 => ScalarType::S16x2,
            Token::DotS32 => ScalarType::S32,
            Token::DotS64 => ScalarType::S64,
            Token::DotU8 => ScalarType::U8,
            Token::DotU16 => ScalarType::U16,
            Token::DotU16x2 => ScalarType::U16x2,
            Token::DotU32 => ScalarType::U32,
            Token::DotU64 => ScalarType::U64,
            Token::DotB8 => ScalarType::B8,
            Token::DotB16 => ScalarType::B16,
            Token::DotB32 => ScalarType::B32,
            Token::DotB64 => ScalarType::B64,
            Token::DotB128 => ScalarType::B128,
            Token::DotPred => ScalarType::Pred,
            Token::DotF16 => ScalarType::F16,
            Token::DotF16x2 => ScalarType::F16x2,
            Token::DotF32 => ScalarType::F32,
            Token::DotF64 => ScalarType::F64,
            Token::DotBF16 => ScalarType::BF16,
            Token::DotBF16x2 => ScalarType::BF16x2,
            _ => return None,
        })
    })
    .parse_next(stream)
}

fn predicated_instruction<'a, 'input>(
    stream: &mut PtxParser<'a, 'input>,
) -> PResult<ast::Statement<ParsedOperandStr<'input>>> {
    (opt(pred_at), parse_instruction, Token::Semicolon)
        .map(|(p, i, _)| ast::Statement::Instruction(p, i))
        .parse_next(stream)
}

fn pred_at<'a, 'input>(stream: &mut PtxParser<'a, 'input>) -> PResult<ast::PredAt<&'input str>> {
    (Token::At, opt(Token::Not), ident)
        .map(|(_, not, label)| ast::PredAt {
            not: not.is_some(),
            label,
        })
        .parse_next(stream)
}

fn label<'a, 'input>(
    stream: &mut PtxParser<'a, 'input>,
) -> PResult<ast::Statement<ParsedOperandStr<'input>>> {
    terminated(ident, Token::Colon)
        .map(|l| ast::Statement::Label(l))
        .parse_next(stream)
}

fn debug_directive<'a, 'input>(stream: &mut PtxParser<'a, 'input>) -> PResult<()> {
    (
        Token::DotLoc,
        u32,
        u32,
        u32,
        opt((
            Token::Comma,
            ident_literal("function_name"),
            ident,
            dispatch! { any;
                Token::Comma => (ident_literal("inlined_at"), u32, u32, u32).void(),
                Token::Plus => (u32, Token::Comma, ident_literal("inlined_at"), u32, u32, u32).void(),
                _ => fail
            },
        )),
    )
        .void()
        .parse_next(stream)
}

fn block_statement<'a, 'input>(
    stream: &mut PtxParser<'a, 'input>,
) -> PResult<ast::Statement<ParsedOperandStr<'input>>> {
    delimited(Token::LBrace, repeat_without_none(statement), Token::RBrace)
        .map(|s| ast::Statement::Block(s))
        .parse_next(stream)
}

fn repeat_without_none<Input: Stream, Output, Error: ParserError<Input>>(
    parser: impl Parser<Input, Option<Output>, Error>,
) -> impl Parser<Input, Vec<Output>, Error> {
    repeat(0.., parser).fold(Vec::new, |mut acc: Vec<_>, item| {
        if let Some(item) = item {
            acc.push(item);
        }
        acc
    })
}

fn ident_literal<
    'a,
    'input,
    I: Stream<Token = Token<'input>> + StreamIsPartial,
    E: ParserError<I>,
>(
    s: &'input str,
) -> impl Parser<I, (), E> + 'input {
    move |stream: &mut I| {
        any.verify(|t| matches!(t, Token::Ident(text) if *text == s))
            .void()
            .parse_next(stream)
    }
}

fn u8_literal<'a, 'input>(x: u8) -> impl Parser<PtxParser<'a, 'input>, (), ContextError> {
    move |stream: &mut PtxParser| u8.verify(|t| *t == x).void().parse_next(stream)
}

impl<Ident> ast::ParsedOperand<Ident> {
    fn parse<'a, 'input>(
        stream: &mut PtxParser<'a, 'input>,
    ) -> PResult<ast::ParsedOperand<&'input str>> {
        use winnow::combinator::*;
        use winnow::token::any;
        fn vector_index<'input>(inp: &'input str) -> Result<u8, PtxError> {
            match inp {
                "x" | "r" => Ok(0),
                "y" | "g" => Ok(1),
                "z" | "b" => Ok(2),
                "w" | "a" => Ok(3),
                _ => Err(PtxError::WrongVectorElement),
            }
        }
        fn ident_operands<'a, 'input>(
            stream: &mut PtxParser<'a, 'input>,
        ) -> PResult<ast::ParsedOperand<&'input str>> {
            let main_ident = ident.parse_next(stream)?;
            alt((
                preceded(Token::Plus, s32)
                    .map(move |offset| ast::ParsedOperand::RegOffset(main_ident, offset)),
                take_error(preceded(Token::Dot, ident).map(move |suffix| {
                    let vector_index = vector_index(suffix)
                        .map_err(move |e| (ast::ParsedOperand::VecMember(main_ident, 0), e))?;
                    Ok(ast::ParsedOperand::VecMember(main_ident, vector_index))
                })),
                empty.value(ast::ParsedOperand::Reg(main_ident)),
            ))
            .parse_next(stream)
        }
        fn vector_operand<'a, 'input>(
            stream: &mut PtxParser<'a, 'input>,
        ) -> PResult<Vec<&'input str>> {
            let (_, r1, _, r2) =
                (Token::LBracket, ident, Token::Comma, ident).parse_next(stream)?;
            dispatch! {any;
                Token::LBracket => empty.map(|_| vec![r1, r2]),
                Token::Comma => (ident, Token::Comma, ident, Token::LBracket).map(|(r3, _, r4, _)| vec![r1, r2, r3, r4]),
                _ => fail
            }
            .parse_next(stream)
        }
        alt((
            ident_operands,
            immediate_value.map(ast::ParsedOperand::Imm),
            vector_operand.map(ast::ParsedOperand::VecPack),
        ))
        .parse_next(stream)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PtxError {
    #[error("{source}")]
    ParseInt {
        #[from]
        source: ParseIntError,
    },
    #[error("{source}")]
    ParseFloat {
        #[from]
        source: ParseFloatError,
    },
    #[error("")]
    Todo,
    #[error("")]
    SyntaxError,
    #[error("")]
    NonF32Ftz,
    #[error("")]
    WrongArrayType,
    #[error("")]
    WrongVectorElement,
    #[error("")]
    MultiArrayVariable,
    #[error("")]
    ZeroDimensionArray,
    #[error("")]
    ArrayInitalizer,
    #[error("")]
    NonExternPointer,
    #[error("{start}:{end}")]
    UnrecognizedStatement { start: usize, end: usize },
    #[error("{start}:{end}")]
    UnrecognizedDirective { start: usize, end: usize },
}

#[derive(Debug)]
struct ReverseStream<'a, T>(pub &'a [T]);

impl<'i, T> Stream for ReverseStream<'i, T>
where
    T: Clone + ::std::fmt::Debug,
{
    type Token = T;
    type Slice = &'i [T];

    type IterOffsets =
        std::iter::Enumerate<std::iter::Cloned<std::iter::Rev<std::slice::Iter<'i, T>>>>;

    type Checkpoint = &'i [T];

    fn iter_offsets(&self) -> Self::IterOffsets {
        self.0.iter().rev().cloned().enumerate()
    }

    fn eof_offset(&self) -> usize {
        self.0.len()
    }

    fn next_token(&mut self) -> Option<Self::Token> {
        let (token, next) = self.0.split_last()?;
        self.0 = next;
        Some(token.clone())
    }

    fn offset_for<P>(&self, predicate: P) -> Option<usize>
    where
        P: Fn(Self::Token) -> bool,
    {
        self.0.iter().rev().position(|b| predicate(b.clone()))
    }

    fn offset_at(&self, tokens: usize) -> Result<usize, winnow::error::Needed> {
        if let Some(needed) = tokens
            .checked_sub(self.0.len())
            .and_then(std::num::NonZeroUsize::new)
        {
            Err(winnow::error::Needed::Size(needed))
        } else {
            Ok(tokens)
        }
    }

    fn next_slice(&mut self, offset: usize) -> Self::Slice {
        let offset = self.0.len() - offset;
        let (next, slice) = self.0.split_at(offset);
        self.0 = next;
        slice
    }

    fn checkpoint(&self) -> Self::Checkpoint {
        self.0
    }

    fn reset(&mut self, checkpoint: &Self::Checkpoint) {
        self.0 = checkpoint;
    }

    fn raw(&self) -> &dyn std::fmt::Debug {
        self
    }
}

impl<'a, T> Offset<&'a [T]> for ReverseStream<'a, T> {
    fn offset_from(&self, start: &&'a [T]) -> usize {
        let fst = start.as_ptr();
        let snd = self.0.as_ptr();

        debug_assert!(
            snd <= fst,
            "`Offset::offset_from({snd:?}, {fst:?})` only accepts slices of `self`"
        );
        (fst as usize - snd as usize) / std::mem::size_of::<T>()
    }
}

impl<'a, T> StreamIsPartial for ReverseStream<'a, T> {
    type PartialState = ();

    fn complete(&mut self) -> Self::PartialState {}

    fn restore_partial(&mut self, _state: Self::PartialState) {}

    fn is_partial_supported() -> bool {
        false
    }
}

impl<'input, I: Stream<Token = Self> + StreamIsPartial, E: ParserError<I>> Parser<I, Self, E>
    for Token<'input>
{
    fn parse_next(&mut self, input: &mut I) -> PResult<Self, E> {
        any.verify(|t| t == self).parse_next(input)
    }
}

// Modifiers are turned into arguments to the blocks, with type:
// * If it is an alternative:
//   * If it is mandatory then its type is Foo (as defined by the relevant rule)
//   * If it is optional then its type is Option<Foo>
// * Otherwise:
//   * If it is mandatory then it is skipped
//   * If it is optional then its type is `bool`

type ParsedOperandStr<'input> = ast::ParsedOperand<&'input str>;

derive_parser!(
    #[derive(Logos, PartialEq, Eq, Debug, Clone, Copy)]
    #[logos(skip r"\s+")]
    enum Token<'input> {
        #[token(",")]
        Comma,
        #[token(".")]
        Dot,
        #[token(":")]
        Colon,
        #[token(";")]
        Semicolon,
        #[token("@")]
        At,
        #[regex(r"[a-zA-Z][a-zA-Z0-9_$]*|[_$%][a-zA-Z0-9_$]+", |lex| lex.slice(), priority = 0)]
        Ident(&'input str),
        #[regex(r#""[^"]*""#)]
        String,
        #[token("|")]
        Or,
        #[token("!")]
        Not,
        #[token("(")]
        LParen,
        #[token(")")]
        RParen,
        #[token("[")]
        LBracket,
        #[token("]")]
        RBracket,
        #[token("{")]
        LBrace,
        #[token("}")]
        RBrace,
        #[token("<")]
        Lt,
        #[token(">")]
        Gt,
        #[regex(r"0[fF][0-9a-zA-Z]{8}", |lex| lex.slice())]
        F32(&'input str),
        #[regex(r"0[dD][0-9a-zA-Z]{16}", |lex| lex.slice())]
        F64(&'input str),
        #[regex(r"0[xX][0-9a-zA-Z]+U?", |lex| lex.slice())]
        Hex(&'input str),
        #[regex(r"[0-9]+U?", |lex| lex.slice())]
        Decimal(&'input str),
        #[token("-")]
        Minus,
        #[token("+")]
        Plus,
        #[token(".version")]
        DotVersion,
        #[token(".loc")]
        DotLoc,
        #[token(".reg")]
        DotReg,
        #[token(".align")]
        DotAlign,
        #[token(".pragma")]
        DotPragma,
        #[token(".maxnreg")]
        DotMaxnreg,
        #[token(".maxntid")]
        DotMaxntid,
        #[token(".reqntid")]
        DotReqntid,
        #[token(".minnctapersm")]
        DotMinnctapersm,
        #[token(".entry")]
        DotEntry,
        #[token(".func")]
        DotFunc,
        #[token(".extern")]
        DotExtern,
        #[token(".visible")]
        DotVisible,
        #[token(".target")]
        DotTarget,
        #[token(".address_size")]
        DotAddressSize
    }

    #[derive(Copy, Clone, PartialEq, Eq, Hash)]
    pub enum StateSpace {
        Reg,
        Generic,
    }

    #[derive(Copy, Clone, PartialEq, Eq, Hash)]
    pub enum MemScope { }

    #[derive(Copy, Clone, PartialEq, Eq, Hash)]
    pub enum ScalarType { }

    // https://docs.nvidia.com/cuda/parallel-thread-execution/index.html#data-movement-and-conversion-instructions-mov
    mov{.vec}.type  d, a => {
        Instruction::Mov {
            data: ast::MovDetails::new(vec, type_),
            arguments: MovArgs { dst: d, src: a },
        }
    }
    .vec: VectorPrefix = { .v2, .v4 };
    .type: ScalarType =  { .pred,
                           .b16, .b32, .b64,
                           .u16, .u32, .u64,
                           .s16, .s32, .s64,
                                 .f32, .f64 };

    // https://docs.nvidia.com/cuda/parallel-thread-execution/#data-movement-and-conversion-instructions-st
    st{.weak}{.ss}{.cop}{.level::eviction_priority}{.level::cache_hint}{.vec}.type  [a], b{, cache_policy} => {
        if level_eviction_priority.is_some() || level_cache_hint || cache_policy.is_some() {
            state.push(PtxError::Todo);
        }
        Instruction::St {
            data: StData {
                qualifier: weak.unwrap_or(RawLdStQualifier::Weak).into(),
                state_space: ss.unwrap_or(StateSpace::Generic),
                caching: cop.unwrap_or(RawStCacheOperator::Wb).into(),
                typ: ast::Type::maybe_vector(vec, type_)
            },
            arguments: StArgs { src1:a, src2:b }
        }
    }
    st.volatile{.ss}{.vec}.type                                                     [a], b => {
        Instruction::St {
            data: StData {
                qualifier: volatile.into(),
                state_space: ss.unwrap_or(StateSpace::Generic),
                caching: ast::StCacheOperator::Writeback,
                typ: ast::Type::maybe_vector(vec, type_)
            },
            arguments: StArgs { src1:a, src2:b }
        }
    }
    st.relaxed.scope{.ss}{.level::eviction_priority}{.level::cache_hint}{.vec}.type [a], b{, cache_policy} => {
        if level_eviction_priority.is_some() || level_cache_hint || cache_policy.is_some() {
            state.push(PtxError::Todo);
        }
        Instruction::St {
            data: StData {
                qualifier: ast::LdStQualifier::Relaxed(scope),
                state_space: ss.unwrap_or(StateSpace::Generic),
                caching: ast::StCacheOperator::Writeback,
                typ: ast::Type::maybe_vector(vec, type_)
            },
            arguments: StArgs { src1:a, src2:b }
        }
    }
    st.release.scope{.ss}{.level::eviction_priority}{.level::cache_hint}{.vec}.type [a], b{, cache_policy} => {
        if level_eviction_priority.is_some() || level_cache_hint || cache_policy.is_some() {
            state.push(PtxError::Todo);
        }
        Instruction::St {
            data: StData {
                qualifier: ast::LdStQualifier::Release(scope),
                state_space: ss.unwrap_or(StateSpace::Generic),
                caching: ast::StCacheOperator::Writeback,
                typ: ast::Type::maybe_vector(vec, type_)
            },
            arguments: StArgs { src1:a, src2:b }
        }
    }
    st.mmio.relaxed.sys{.global}.type                                               [a], b => {
        state.push(PtxError::Todo);
        Instruction::St {
            data: ast::StData {
                qualifier: ast::LdStQualifier::Relaxed(MemScope::Sys),
                state_space: global.unwrap_or(StateSpace::Generic),
                caching: ast::StCacheOperator::Writeback,
                typ: type_.into()
            },
            arguments: ast::StArgs { src1:a, src2:b }
        }
    }

    .ss: StateSpace =           { .global, .local, .param{::func}, .shared{::cta, ::cluster} };
    .level::eviction_priority: EvictionPriority =
                                { .L1::evict_normal, .L1::evict_unchanged, .L1::evict_first, .L1::evict_last, .L1::no_allocate };
    .level::cache_hint =        { .L2::cache_hint };
    .cop: RawStCacheOperator =  { .wb, .cg, .cs, .wt };
    .scope: MemScope =          { .cta, .cluster, .gpu, .sys };
    .vec: VectorPrefix =        { .v2, .v4 };
    .type: ScalarType =         { .b8, .b16, .b32, .b64, .b128,
                                  .u8, .u16, .u32, .u64,
                                  .s8, .s16, .s32, .s64,
                                  .f32, .f64 };
    RawLdStQualifier =          { .weak, .volatile };
    StateSpace =                { .global };

    // https://docs.nvidia.com/cuda/parallel-thread-execution/#data-movement-and-conversion-instructions-ld
    ld{.weak}{.ss}{.cop}{.level::eviction_priority}{.level::cache_hint}{.level::prefetch_size}{.vec}.type   d, [a]{.unified}{, cache_policy} => {
        let (a, unified) = a;
        if level_eviction_priority.is_some() || level_cache_hint || level_prefetch_size.is_some() || unified || cache_policy.is_some() {
            state.push(PtxError::Todo);
        }
        Instruction::Ld {
            data: LdDetails {
                qualifier: weak.unwrap_or(RawLdStQualifier::Weak).into(),
                state_space: ss.unwrap_or(StateSpace::Generic),
                caching: cop.unwrap_or(RawLdCacheOperator::Ca).into(),
                typ: ast::Type::maybe_vector(vec, type_),
                non_coherent: false
            },
            arguments: LdArgs { dst:d, src:a }
        }
    }
    ld.volatile{.ss}{.level::prefetch_size}{.vec}.type                                                      d, [a] => {
        if level_prefetch_size.is_some() {
            state.push(PtxError::Todo);
        }
        Instruction::Ld {
            data: LdDetails {
                qualifier: volatile.into(),
                state_space: ss.unwrap_or(StateSpace::Generic),
                caching: ast::LdCacheOperator::Cached,
                typ: ast::Type::maybe_vector(vec, type_),
                non_coherent: false
            },
            arguments: LdArgs { dst:d, src:a }
        }
    }
    ld.relaxed.scope{.ss}{.level::eviction_priority}{.level::cache_hint}{.level::prefetch_size}{.vec}.type  d, [a]{, cache_policy} => {
        if level_eviction_priority.is_some() || level_cache_hint || level_prefetch_size.is_some() || cache_policy.is_some() {
            state.push(PtxError::Todo);
        }
        Instruction::Ld {
            data: LdDetails {
                qualifier: ast::LdStQualifier::Relaxed(scope),
                state_space: ss.unwrap_or(StateSpace::Generic),
                caching: ast::LdCacheOperator::Cached,
                typ: ast::Type::maybe_vector(vec, type_),
                non_coherent: false
            },
            arguments: LdArgs { dst:d, src:a }
        }
    }
    ld.acquire.scope{.ss}{.level::eviction_priority}{.level::cache_hint}{.level::prefetch_size}{.vec}.type  d, [a]{, cache_policy} => {
        if level_eviction_priority.is_some() || level_cache_hint || level_prefetch_size.is_some() || cache_policy.is_some() {
            state.push(PtxError::Todo);
        }
        Instruction::Ld {
            data: LdDetails {
                qualifier: ast::LdStQualifier::Acquire(scope),
                state_space: ss.unwrap_or(StateSpace::Generic),
                caching: ast::LdCacheOperator::Cached,
                typ: ast::Type::maybe_vector(vec, type_),
                non_coherent: false
            },
            arguments: LdArgs { dst:d, src:a }
        }
    }
    ld.mmio.relaxed.sys{.global}.type                                                                       d, [a] => {
        state.push(PtxError::Todo);
        Instruction::Ld {
            data: LdDetails {
                qualifier: ast::LdStQualifier::Relaxed(MemScope::Sys),
                state_space: global.unwrap_or(StateSpace::Generic),
                caching: ast::LdCacheOperator::Cached,
                typ: type_.into(),
                non_coherent: false
            },
            arguments: LdArgs { dst:d, src:a }
        }
    }

    .ss: StateSpace =                       { .const, .global, .local, .param{::entry, ::func}, .shared{::cta, ::cluster} };
    .cop: RawLdCacheOperator =              { .ca, .cg, .cs, .lu, .cv };
    .level::eviction_priority: EvictionPriority =
                                            { .L1::evict_normal, .L1::evict_unchanged, .L1::evict_first, .L1::evict_last, .L1::no_allocate };
    .level::cache_hint =                    { .L2::cache_hint };
    .level::prefetch_size: PrefetchSize =   { .L2::64B, .L2::128B, .L2::256B };
    .scope: MemScope =                      { .cta, .cluster, .gpu, .sys };
    .vec: VectorPrefix =                    { .v2, .v4 };
    .type: ScalarType =                     { .b8, .b16, .b32, .b64, .b128,
                                              .u8, .u16, .u32, .u64,
                                              .s8, .s16, .s32, .s64,
                                              .f32, .f64 };
    RawLdStQualifier =                      { .weak, .volatile };
    StateSpace =                            { .global };

    // https://docs.nvidia.com/cuda/parallel-thread-execution/#integer-arithmetic-instructions-add
    add.type        d, a, b => {
        Instruction::Add {
            data: ast::ArithDetails::Integer(
                ast::ArithInteger {
                    type_,
                    saturate: false
                }
            ),
            arguments: AddArgs {
                dst: d, src1: a, src2: b
            }
        }
    }
    add{.sat}.s32   d, a, b => {
        Instruction::Add {
            data: ast::ArithDetails::Integer(
                ast::ArithInteger {
                    type_: s32,
                    saturate: sat
                }
            ),
            arguments: AddArgs {
                dst: d, src1: a, src2: b
            }
        }
    }

    .type: ScalarType = { .u16, .u32, .u64,
                          .s16, .s64,
                          .u16x2, .s16x2 };
    ScalarType =        { .s32 };

    // https://docs.nvidia.com/cuda/parallel-thread-execution/#floating-point-instructions-add
    add{.rnd}{.ftz}{.sat}.f32  d, a, b => {
        Instruction::Add {
            data: ast::ArithDetails::Float(
                ast::ArithFloat {
                    type_: f32,
                    rounding: rnd.map(Into::into),
                    flush_to_zero: Some(ftz),
                    saturate: sat
                }
            ),
            arguments: AddArgs {
                dst: d, src1: a, src2: b
            }
        }
    }
    add{.rnd}.f64              d, a, b => {
        Instruction::Add {
            data: ast::ArithDetails::Float(
                ast::ArithFloat {
                    type_: f64,
                    rounding: rnd.map(Into::into),
                    flush_to_zero: None,
                    saturate: false
                }
            ),
            arguments: AddArgs {
                dst: d, src1: a, src2: b
            }
        }
    }

    .rnd: RawFloatRounding = { .rn, .rz, .rm, .rp };
    ScalarType =        { .f32, .f64 };

    // https://docs.nvidia.com/cuda/parallel-thread-execution/#half-precision-floating-point-instructions-add
    add{.rnd}{.ftz}{.sat}.f16   d, a, b => {
        Instruction::Add {
            data: ast::ArithDetails::Float(
                ast::ArithFloat {
                    type_: f16,
                    rounding: rnd.map(Into::into),
                    flush_to_zero: Some(ftz),
                    saturate: sat
                }
            ),
            arguments: AddArgs {
                dst: d, src1: a, src2: b
            }
        }
    }
    add{.rnd}{.ftz}{.sat}.f16x2 d, a, b => {
        Instruction::Add {
            data: ast::ArithDetails::Float(
                ast::ArithFloat {
                    type_: f16x2,
                    rounding: rnd.map(Into::into),
                    flush_to_zero: Some(ftz),
                    saturate: sat
                }
            ),
            arguments: AddArgs {
                dst: d, src1: a, src2: b
            }
        }
    }
    add{.rnd}.bf16              d, a, b => {
        Instruction::Add {
            data: ast::ArithDetails::Float(
                ast::ArithFloat {
                    type_: bf16,
                    rounding: rnd.map(Into::into),
                    flush_to_zero: None,
                    saturate: false
                }
            ),
            arguments: AddArgs {
                dst: d, src1: a, src2: b
            }
        }
    }
    add{.rnd}.bf16x2            d, a, b => {
        Instruction::Add {
            data: ast::ArithDetails::Float(
                ast::ArithFloat {
                    type_: bf16x2,
                    rounding: rnd.map(Into::into),
                    flush_to_zero: None,
                    saturate: false
                }
            ),
            arguments: AddArgs {
                dst: d, src1: a, src2: b
            }
        }
    }

    .rnd: RawFloatRounding = { .rn };
    ScalarType =        { .f16, .f16x2, .bf16, .bf16x2 };

    ret{.uni} => {
        Instruction::Ret { data: RetData { uniform: uni } }
    }

);

fn main() {
    use winnow::combinator::*;
    use winnow::token::*;
    use winnow::Parser;

    println!("{}", mem::size_of::<Token>());

    let mut input: &[Token] = &[][..];
    let x = opt(any::<_, ContextError>.verify_map(|_| {
        println!("MAP");
        Some(true)
    }))
    .parse_next(&mut input)
    .unwrap();
    dbg!(x);
    let lexer = Token::lexer(
        "
        .version 6.5
        .target sm_30
        .address_size 64
        
        .visible .entry add(
            .param .u64 input,
            .param .u64 output
        )
        {
            .reg .u64 	    in_addr;
            .reg .u64 	    out_addr;
            .reg .u64 	    temp;
            .reg .u64 	    temp2;
        
            ld.param.u64 	in_addr, [input];
            ld.param.u64 	out_addr, [output];
        
            ld.u64          temp, [in_addr];
            add.u64		    temp2, temp, 1;
            st.u64          [out_addr], temp2;
            ret;
        }
        
        ",
    );
    let tokens = lexer.map(|t| t.unwrap()).collect::<Vec<_>>();
    println!("{:?}", &tokens);
    let stream = PtxParser {
        input: &tokens[..],
        state: Vec::new(),
    };
    let module_ = module.parse(stream).unwrap();
    println!("{}", mem::size_of::<Token>());
}

#[cfg(test)]
mod tests {
    use super::target;
    use super::Token;
    use logos::Logos;
    use winnow::prelude::*;

    #[test]
    fn sm_11() {
        let tokens = Token::lexer(".target sm_11")
            .collect::<Result<Vec<_>, ()>>()
            .unwrap();
        let stream = super::PtxParser {
            input: &tokens[..],
            state: Vec::new(),
        };
        assert_eq!(target.parse(stream).unwrap(), (11, None));
    }

    #[test]
    fn sm_90a() {
        let tokens = Token::lexer(".target sm_90a")
            .collect::<Result<Vec<_>, ()>>()
            .unwrap();
        let stream = super::PtxParser {
            input: &tokens[..],
            state: Vec::new(),
        };
        assert_eq!(target.parse(stream).unwrap(), (90, Some('a')));
    }

    #[test]
    fn sm_90ab() {
        let tokens = Token::lexer(".target sm_90ab")
            .collect::<Result<Vec<_>, ()>>()
            .unwrap();
        let stream = super::PtxParser {
            input: &tokens[..],
            state: Vec::new(),
        };
        assert!(target.parse(stream).is_err());
    }
}