//! ShaderLab as a tree of commands. A command is a keyword of its block,
//! the values that follow it and, for some, a block of its own. ShaderLab
//! knows no line ends: `Blend Off Lighting Off Cull Off` is three commands,
//! told apart by the keywords each block knows. Properties and tags have
//! grammars of their own.
use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Ident(String),
    Str(String),
    Num(String),
    /// What `[...]` holds: a property reference or an attribute.
    Bracket(String),
    Punct(char),
}

impl Token {
    pub fn text(&self) -> String {
        match self {
            Token::Ident(text) | Token::Str(text) | Token::Num(text) | Token::Bracket(text) => {
                text.clone()
            }
            Token::Punct(c) => c.to_string(),
        }
    }

    fn is_ident(&self, word: &str) -> bool {
        matches!(self, Token::Ident(text) if text.eq_ignore_ascii_case(word))
    }
}

pub fn tokens(text: &str) -> Result<Vec<Token>> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let c = chars[at];
        if c.is_whitespace() || c == '\u{feff}' {
            at += 1;
        } else if c == '/' && chars.get(at + 1) == Some(&'/') {
            while at < chars.len() && chars[at] != '\n' {
                at += 1;
            }
        } else if c == '/' && chars.get(at + 1) == Some(&'*') {
            at += 2;
            while at + 1 < chars.len() && !(chars[at] == '*' && chars[at + 1] == '/') {
                at += 1;
            }
            at += 2;
        } else if c == '#' {
            // `#LINE n`, which the compiler leaves where a program was.
            while at < chars.len() && chars[at] != '\n' {
                at += 1;
            }
        } else if c == '"' {
            let start = at + 1;
            at += 1;
            while at < chars.len() && chars[at] != '"' {
                at += 1;
            }
            out.push(Token::Str(
                chars[start..at.min(chars.len())].iter().collect(),
            ));
            at += 1;
        } else if c == '[' {
            let start = at + 1;
            let mut depth = 1;
            at += 1;
            while at < chars.len() && depth > 0 {
                match chars[at] {
                    '[' => depth += 1,
                    ']' => depth -= 1,
                    _ => {}
                }
                at += 1;
            }
            out.push(Token::Bracket(
                chars[start..at.saturating_sub(1)]
                    .iter()
                    .collect::<String>()
                    .trim()
                    .to_string(),
            ));
        } else if c.is_ascii_digit()
            || ((c == '-' || c == '+' || c == '.')
                && chars
                    .get(at + 1)
                    .is_some_and(|n| n.is_ascii_digit() || *n == '.'))
        {
            let start = at;
            at += 1;
            while at < chars.len() {
                let n = chars[at];
                let exponent = (n == '-' || n == '+') && matches!(chars[at - 1], 'e' | 'E');
                if n.is_ascii_alphanumeric() || n == '.' || exponent {
                    at += 1;
                } else {
                    break;
                }
            }
            out.push(Token::Num(chars[start..at].iter().collect()));
        } else if c.is_alphabetic() || c == '_' {
            let start = at;
            while at < chars.len() && (chars[at].is_alphanumeric() || chars[at] == '_') {
                at += 1;
            }
            out.push(Token::Ident(chars[start..at].iter().collect()));
        } else {
            out.push(Token::Punct(c));
            at += 1;
        }
    }
    Ok(out)
}

/// A command: its keyword as written, its values and its block.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Command {
    pub name: String,
    pub args: Vec<Token>,
    pub block: Option<Vec<Command>>,
    /// A property, for a command of `Properties`.
    pub property: Option<Property>,
}

impl Command {
    pub fn is(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Property {
    pub attributes: Vec<String>,
    pub name: String,
    pub description: String,
    /// The type and its arguments (`Range(0, 1)`).
    pub kind: String,
    pub kind_args: Vec<String>,
    /// The default: numbers, or a texture's name.
    pub default: Vec<String>,
    pub texture: Option<String>,
}

/// The keywords each block knows, by the block's keyword.
fn keywords(block: &str) -> &'static [&'static str] {
    const STATE: &[&str] = &[
        "Cull",
        "ZTest",
        "ZWrite",
        "ZClip",
        "Blend",
        "BlendOp",
        "ColorMask",
        "Offset",
        "AlphaToMask",
        "Conservative",
        "Stencil",
        "Fog",
        "Lighting",
        "SeparateSpecular",
        "ColorMaterial",
        "Material",
        "AlphaTest",
        "SetTexture",
        "BindChannels",
        "Tags",
        "LOD",
        "Name",
        "GpuProgramID",
        "PackageRequirements",
    ];
    match block.to_ascii_lowercase().as_str() {
        "shader" => &[
            "Properties",
            "SubShader",
            "Category",
            "Fallback",
            "CustomEditor",
            "CustomEditorForRenderPipeline",
            "Dependency",
        ],
        "category" | "subshader" => &[
            "SubShader",
            "Category",
            "Fallback",
            "CustomEditor",
            "Pass",
            "UsePass",
            "GrabPass",
            "Cull",
            "ZTest",
            "ZWrite",
            "ZClip",
            "Blend",
            "BlendOp",
            "ColorMask",
            "Offset",
            "AlphaToMask",
            "Conservative",
            "Stencil",
            "Fog",
            "Lighting",
            "SeparateSpecular",
            "ColorMaterial",
            "Material",
            "AlphaTest",
            "SetTexture",
            "BindChannels",
            "Tags",
            "LOD",
            "Name",
            "PackageRequirements",
        ],
        "pass" | "grabpass" => STATE,
        "stencil" => &[
            "Ref",
            "ReadMask",
            "WriteMask",
            "Comp",
            "Pass",
            "Fail",
            "ZFail",
            "CompFront",
            "PassFront",
            "FailFront",
            "ZFailFront",
            "CompBack",
            "PassBack",
            "FailBack",
            "ZFailBack",
        ],
        "fog" => &["Mode", "Color", "Density", "Range"],
        "material" => &["Diffuse", "Ambient", "Specular", "Shininess", "Emission"],
        "settexture" => &["Combine", "ConstantColor", "Matrix"],
        "bindchannels" => &["Bind"],
        _ => &[],
    }
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.at).cloned();
        self.at += 1;
        token
    }

    fn expect(&mut self, c: char) -> Result<()> {
        match self.next() {
            Some(Token::Punct(p)) if p == c => Ok(()),
            other => bail!("ShaderLab: expected '{c}', found {other:?}"),
        }
    }

    fn is_keyword(&self, block: &str, token: &Token) -> bool {
        keywords(block).iter().any(|k| token.is_ident(k))
    }

    /// The commands of a block, up to its `}`.
    fn block(&mut self, kind: &str) -> Result<Vec<Command>> {
        let mut commands = Vec::new();
        loop {
            match self.peek() {
                None => bail!("ShaderLab: {kind} block is not closed"),
                Some(Token::Punct('}')) => {
                    self.at += 1;
                    return Ok(commands);
                }
                _ => {}
            }
            let lower = kind.to_ascii_lowercase();
            if lower == "properties" {
                commands.push(self.property()?);
                continue;
            }
            if lower == "tags" || lower == "packagerequirements" {
                commands.push(self.pair()?);
                continue;
            }
            let Some(Token::Ident(name)) = self.next() else {
                bail!(
                    "ShaderLab: a command of {kind} begins with {:?}",
                    self.tokens.get(self.at - 1)
                );
            };
            let mut command = Command {
                name: name.clone(),
                ..Default::default()
            };
            // A block keyword names the grammar of its own block.
            let inner = name.clone();
            loop {
                match self.peek() {
                    None | Some(Token::Punct('}')) => break,
                    Some(Token::Punct('{')) => {
                        self.at += 1;
                        command.block = Some(self.block(&inner)?);
                        break;
                    }
                    Some(token) if self.is_keyword(kind, token) && !command.args.is_empty() => {
                        // Two values a comma joins belong together.
                        if matches!(command.args.last(), Some(Token::Punct(','))) {
                            command.args.push(self.next().unwrap());
                            continue;
                        }
                        break;
                    }
                    Some(token)
                        if self.is_keyword(kind, token)
                            && command.args.is_empty()
                            && !takes_keyword_value(&name) =>
                    {
                        break;
                    }
                    _ => command.args.push(self.next().unwrap()),
                }
            }
            commands.push(command);
        }
    }

    /// `"key" = "value"`.
    fn pair(&mut self) -> Result<Command> {
        let key = self.next().map(|t| t.text()).unwrap_or_default();
        self.expect('=')?;
        let value = self.next().map(|t| t.text()).unwrap_or_default();
        Ok(Command {
            name: key,
            args: vec![Token::Str(value)],
            ..Default::default()
        })
    }

    /// `[Attribute]... _Name ("Description", Type) = default {...}`.
    fn property(&mut self) -> Result<Command> {
        let mut property = Property::default();
        while let Some(Token::Bracket(attribute)) = self.peek() {
            property.attributes.push(attribute.clone());
            self.at += 1;
        }
        property.name = match self.next() {
            Some(Token::Ident(name)) => name,
            other => bail!("ShaderLab: a property is named by {other:?}"),
        };
        self.expect('(')?;
        property.description = match self.next() {
            Some(Token::Str(text)) => text,
            other => bail!(
                "ShaderLab: property {} is described by {other:?}",
                property.name
            ),
        };
        self.expect(',')?;
        property.kind = self.next().map(|t| t.text()).unwrap_or_default();
        if matches!(self.peek(), Some(Token::Punct('('))) {
            self.at += 1;
            while let Some(token) = self.next() {
                match token {
                    Token::Punct(')') => break,
                    Token::Punct(',') => {}
                    other => property.kind_args.push(other.text()),
                }
            }
        }
        self.expect(')')?;
        self.expect('=')?;
        match self.next() {
            Some(Token::Punct('(')) => {
                while let Some(token) = self.next() {
                    match token {
                        Token::Punct(')') => break,
                        Token::Punct(',') => {}
                        other => property.default.push(other.text()),
                    }
                }
            }
            Some(Token::Str(name)) => property.texture = Some(name),
            Some(Token::Num(number)) => property.default.push(number),
            Some(Token::Punct('-')) => {
                let number = self.next().map(|t| t.text()).unwrap_or_default();
                property.default.push(format!("-{number}"));
            }
            other => bail!(
                "ShaderLab: property {} defaults to {other:?}",
                property.name
            ),
        }
        // A texture's options block, empty since 5.0.
        if matches!(self.peek(), Some(Token::Punct('{'))) {
            while let Some(token) = self.next() {
                if token == Token::Punct('}') {
                    break;
                }
            }
        }
        Ok(Command {
            name: property.name.clone(),
            property: Some(property),
            ..Default::default()
        })
    }
}

/// Commands whose value may be a word another command is named by.
fn takes_keyword_value(name: &str) -> bool {
    ["Pass", "Color"]
        .iter()
        .any(|k| name.eq_ignore_ascii_case(k))
}

/// The shader: its name and its commands.
pub fn shader(text: &str) -> Result<(String, Vec<Command>)> {
    let mut parser = Parser {
        tokens: tokens(text)?,
        at: 0,
    };
    if !parser.next().is_some_and(|t| t.is_ident("Shader")) {
        bail!("ShaderLab: the source begins with no Shader");
    }
    let name = match parser.next() {
        Some(Token::Str(name)) => name,
        other => bail!("ShaderLab: the shader is named by {other:?}"),
    };
    parser.expect('{')?;
    let commands = parser.block("Shader")?;
    Ok((name, commands))
}
