mod autolink;
mod inlines;
#[cfg(feature = "shortcodes")]
pub mod shortcodes;
mod table;

use crate::adapters::SyntaxHighlighterAdapter;
use crate::arena_tree::Node;
use crate::ctype::{isdigit, isspace};
use crate::entity;
use crate::nodes;
use crate::nodes::{
    Ast, AstNode, ListDelimType, ListType, NodeCodeBlock, NodeDescriptionItem, NodeHeading,
    NodeHtmlBlock, NodeList, NodeValue,
};
use crate::scanners;
use crate::strings::{self, split_off_front_matter};
use std::cell::RefCell;
use std::cmp::min;
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::mem;
use std::str;
use typed_arena::Arena;

use crate::adapters::HeadingAdapter;

use self::inlines::RefMap;

const TAB_STOP: usize = 4;
const CODE_INDENT: usize = 4;

macro_rules! node_matches {
    ($node:expr, $( $pat:pat )|+) => {{
        matches!(
            $node.data.borrow().value,
            $( $pat )|+
        )
    }};
}

/// Parse a Markdown document to an AST.
///
/// See the documentation of the crate root for an example.
pub fn parse_document<'a>(
    arena: &'a Arena<AstNode<'a>>,
    buffer: &str,
    options: &ComrakOptions,
) -> &'a AstNode<'a> {
    parse_document_with_broken_link_callback(arena, buffer, options, None)
}

/// Parse a Markdown document to an AST.
///
/// In case the parser encounters any potential links that have a broken reference (e.g `[foo]`
/// when there is no `[foo]: url` entry at the bottom) the provided callback will be called with
/// the reference name, and the returned pair will be used as the link destination and title if not
/// None.
///
/// **Note:** The label provided to the callback is the normalized representation of the label as
/// described in the [GFM spec](https://github.github.com/gfm/#matches).
///
/// ```
/// use comrak::{Arena, parse_document_with_broken_link_callback, format_html, ComrakOptions};
/// use comrak::nodes::{AstNode, NodeValue};
///
/// # fn main() -> std::io::Result<()> {
/// // The returned nodes are created in the supplied Arena, and are bound by its lifetime.
/// let arena = Arena::new();
///
/// let root = parse_document_with_broken_link_callback(
///     &arena,
///     "# Cool input!\nWow look at this cool [link][foo]. A [broken link] renders as text.",
///     &ComrakOptions::default(),
///     Some(&mut |link_ref: &str| match link_ref {
///         "foo" => Some((
///             "https://www.rust-lang.org/".to_string(),
///             "The Rust Language".to_string(),
///         )),
///         _ => None,
///     }),
/// );
///
/// let mut output = Vec::new();
/// format_html(root, &ComrakOptions::default(), &mut output)?;
/// let output_str = std::str::from_utf8(&output).expect("invalid UTF-8");
/// assert_eq!(output_str, "<h1>Cool input!</h1>\n<p>Wow look at this cool \
///                 <a href=\"https://www.rust-lang.org/\" title=\"The Rust Language\">link</a>. \
///                 A [broken link] renders as text.</p>\n");
/// # Ok(())
/// # }
/// ```
pub fn parse_document_with_broken_link_callback<'a, 'c>(
    arena: &'a Arena<AstNode<'a>>,
    buffer: &str,
    options: &ComrakOptions,
    callback: Option<Callback<'c>>,
) -> &'a AstNode<'a> {
    let root: &'a AstNode<'a> = arena.alloc(Node::new(RefCell::new(Ast {
        value: NodeValue::Document,
        content: String::new(),
        start_line: 0,
        open: true,
        last_line_blank: false,
        table_visited: false,
    })));
    let mut parser = Parser::new(arena, root, options, callback);
    let mut linebuf = Vec::with_capacity(buffer.len());
    parser.feed(&mut linebuf, buffer, true);
    parser.finish(linebuf)
}

type Callback<'c> = &'c mut dyn FnMut(&str) -> Option<(String, String)>;

pub struct Parser<'a, 'o, 'c> {
    arena: &'a Arena<AstNode<'a>>,
    refmap: RefMap,
    root: &'a AstNode<'a>,
    current: &'a AstNode<'a>,
    line_number: u32,
    offset: usize,
    column: usize,
    thematic_break_kill_pos: usize,
    first_nonspace: usize,
    first_nonspace_column: usize,
    indent: usize,
    blank: bool,
    partially_consumed_tab: bool,
    last_line_length: usize,
    last_buffer_ended_with_cr: bool,
    total_size: usize,
    options: &'o ComrakOptions,
    callback: Option<Callback<'c>>,
}

#[derive(Default, Debug, Clone)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
/// Umbrella options struct.
pub struct ComrakOptions {
    /// Enable CommonMark extensions.
    pub extension: ComrakExtensionOptions,

    /// Configure parse-time options.
    pub parse: ComrakParseOptions,

    /// Configure render-time options.
    pub render: ComrakRenderOptions,
}

#[derive(Default, Debug, Clone)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
/// Options to select extensions.
pub struct ComrakExtensionOptions {
    /// Enables the
    /// [strikethrough extension](https://github.github.com/gfm/#strikethrough-extension-)
    /// from the GFM spec.
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// options.extension.strikethrough = true;
    /// assert_eq!(markdown_to_html("Hello ~world~ there.\n", &options),
    ///            "<p>Hello <del>world</del> there.</p>\n");
    /// ```
    pub strikethrough: bool,

    /// Enables the
    /// [tagfilter extension](https://github.github.com/gfm/#disallowed-raw-html-extension-)
    /// from the GFM spec.
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// options.extension.tagfilter = true;
    /// options.render.unsafe_ = true;
    /// assert_eq!(markdown_to_html("Hello <xmp>.\n\n<xmp>", &options),
    ///            "<p>Hello &lt;xmp>.</p>\n&lt;xmp>\n");
    /// ```
    pub tagfilter: bool,

    /// Enables the [table extension](https://github.github.com/gfm/#tables-extension-)
    /// from the GFM spec.
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// options.extension.table = true;
    /// assert_eq!(markdown_to_html("| a | b |\n|---|---|\n| c | d |\n", &options),
    ///            "<table>\n<thead>\n<tr>\n<th>a</th>\n<th>b</th>\n</tr>\n</thead>\n\
    ///             <tbody>\n<tr>\n<td>c</td>\n<td>d</td>\n</tr>\n</tbody>\n</table>\n");
    /// ```
    pub table: bool,

    /// Enables the [autolink extension](https://github.github.com/gfm/#autolinks-extension-)
    /// from the GFM spec.
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// options.extension.autolink = true;
    /// assert_eq!(markdown_to_html("Hello www.github.com.\n", &options),
    ///            "<p>Hello <a href=\"http://www.github.com\">www.github.com</a>.</p>\n");
    /// ```
    pub autolink: bool,

    /// Enables the
    /// [task list items extension](https://github.github.com/gfm/#task-list-items-extension-)
    /// from the GFM spec.
    ///
    /// Note that the spec does not define the precise output, so only the bare essentials are
    /// rendered.
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// options.extension.tasklist = true;
    /// options.render.unsafe_ = true;
    /// assert_eq!(markdown_to_html("* [x] Done\n* [ ] Not done\n", &options),
    ///            "<ul>\n<li><input type=\"checkbox\" disabled=\"\" checked=\"\" /> Done</li>\n\
    ///            <li><input type=\"checkbox\" disabled=\"\" /> Not done</li>\n</ul>\n");
    /// ```
    pub tasklist: bool,

    /// Enables the superscript Comrak extension.
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// options.extension.superscript = true;
    /// assert_eq!(markdown_to_html("e = mc^2^.\n", &options),
    ///            "<p>e = mc<sup>2</sup>.</p>\n");
    /// ```
    pub superscript: bool,

    /// Enables the header IDs Comrak extension.
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// options.extension.header_ids = Some("user-content-".to_string());
    /// assert_eq!(markdown_to_html("# README\n", &options),
    ///            "<h1><a href=\"#readme\" aria-hidden=\"true\" class=\"anchor\" id=\"user-content-readme\"></a>README</h1>\n");
    /// ```
    pub header_ids: Option<String>,

    /// Enables the footnotes extension per `cmark-gfm`.
    ///
    /// For usage, see `src/tests.rs`.  The extension is modelled after
    /// [Kramdown](https://kramdown.gettalong.org/syntax.html#footnotes).
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// options.extension.footnotes = true;
    /// assert_eq!(markdown_to_html("Hi[^x].\n\n[^x]: A greeting.\n", &options),
    ///            "<p>Hi<sup class=\"footnote-ref\"><a href=\"#fn-1\" id=\"fnref-1\" data-footnote-ref>1</a></sup>.</p>\n<section class=\"footnotes\" data-footnotes>\n<ol>\n<li id=\"fn-1\">\n<p>A greeting. <a href=\"#fnref-1\" class=\"footnote-backref\" data-footnote-backref aria-label=\"Back to content\">↩</a></p>\n</li>\n</ol>\n</section>\n");
    /// ```
    pub footnotes: bool,

    /// Enables the description lists extension.
    ///
    /// Each term must be defined in one paragraph, followed by a blank line,
    /// and then by the details.  Details begins with a colon.
    ///
    /// ``` md
    /// First term
    ///
    /// : Details for the **first term**
    ///
    /// Second term
    ///
    /// : Details for the **second term**
    ///
    ///     More details in second paragraph.
    /// ```
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// options.extension.description_lists = true;
    /// assert_eq!(markdown_to_html("Term\n\n: Definition", &options),
    ///            "<dl><dt>Term</dt>\n<dd>\n<p>Definition</p>\n</dd>\n</dl>\n");
    /// ```
    pub description_lists: bool,

    /// Enables the front matter extension.
    ///
    /// Front matter, which begins with the delimiter string at the beginning of the file and ends
    /// at the end of the next line that contains only the delimiter, is passed through unchanged
    /// in markdown output and omitted from HTML output.
    ///
    /// ``` md
    /// ---
    /// layout: post
    /// title: Formatting Markdown with Comrak
    /// ---
    ///
    /// # Shorter Title
    ///
    /// etc.
    /// ```
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// options.extension.front_matter_delimiter = Some("---".to_owned());
    /// assert_eq!(
    ///     markdown_to_html("---\nlayout: post\n---\nText\n", &options),
    ///     markdown_to_html("Text\n", &ComrakOptions::default()));
    /// ```
    ///
    /// ```
    /// # use comrak::{format_commonmark, Arena, ComrakOptions};
    /// use comrak::parse_document;
    /// let mut options = ComrakOptions::default();
    /// options.extension.front_matter_delimiter = Some("---".to_owned());
    /// let arena = Arena::new();
    /// let input ="---\nlayout: post\n---\nText\n";
    /// let root = parse_document(&arena, input, &options);
    /// let mut buf = Vec::new();
    /// format_commonmark(&root, &options, &mut buf);
    /// assert_eq!(&String::from_utf8(buf).unwrap(), input);
    /// ```
    pub front_matter_delimiter: Option<String>,

    #[cfg(feature = "shortcodes")]
    /// Available if "shortcodes" feature is enabled.  Phrases wrapped inside of ':' blocks will be
    /// replaced with emojis.
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// assert_eq!(markdown_to_html("Happy Friday! :smile:", &options),
    ///            "<p>Happy Friday! :smile:</p>\n");
    ///
    /// options.extension.shortcodes = true;
    /// assert_eq!(markdown_to_html("Happy Friday! :smile:", &options),
    ///            "<p>Happy Friday! 😄</p>\n");
    /// ```
    pub shortcodes: bool,
}

#[derive(Default, Debug, Clone)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
/// Options for parser functions.
pub struct ComrakParseOptions {
    /// Punctuation (quotes, full-stops and hyphens) are converted into 'smart' punctuation.
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// assert_eq!(markdown_to_html("'Hello,' \"world\" ...", &options),
    ///            "<p>'Hello,' &quot;world&quot; ...</p>\n");
    ///
    /// options.parse.smart = true;
    /// assert_eq!(markdown_to_html("'Hello,' \"world\" ...", &options),
    ///            "<p>‘Hello,’ “world” …</p>\n");
    /// ```
    pub smart: bool,

    /// The default info string for fenced code blocks.
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// assert_eq!(markdown_to_html("```\nfn hello();\n```\n", &options),
    ///            "<pre><code>fn hello();\n</code></pre>\n");
    ///
    /// options.parse.default_info_string = Some("rust".into());
    /// assert_eq!(markdown_to_html("```\nfn hello();\n```\n", &options),
    ///            "<pre><code class=\"language-rust\">fn hello();\n</code></pre>\n");
    /// ```
    pub default_info_string: Option<String>,

    /// Whether or not a simple `x` or `X` is used for tasklist or any other symbol is allowed.
    pub relaxed_tasklist_matching: bool,
}

#[derive(Default, Debug, Clone, Copy)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
/// Options for formatter functions.
pub struct ComrakRenderOptions {
    /// [Soft line breaks](http://spec.commonmark.org/0.27/#soft-line-breaks) in the input
    /// translate into hard line breaks in the output.
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// assert_eq!(markdown_to_html("Hello.\nWorld.\n", &options),
    ///            "<p>Hello.\nWorld.</p>\n");
    ///
    /// options.render.hardbreaks = true;
    /// assert_eq!(markdown_to_html("Hello.\nWorld.\n", &options),
    ///            "<p>Hello.<br />\nWorld.</p>\n");
    /// ```
    pub hardbreaks: bool,

    /// GitHub-style `<pre lang="xyz">` is used for fenced code blocks with info tags.
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// assert_eq!(markdown_to_html("``` rust\nfn hello();\n```\n", &options),
    ///            "<pre><code class=\"language-rust\">fn hello();\n</code></pre>\n");
    ///
    /// options.render.github_pre_lang = true;
    /// assert_eq!(markdown_to_html("``` rust\nfn hello();\n```\n", &options),
    ///            "<pre lang=\"rust\"><code>fn hello();\n</code></pre>\n");
    /// ```
    pub github_pre_lang: bool,

    /// Enable full info strings for code blocks
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// assert_eq!(markdown_to_html("``` rust extra info\nfn hello();\n```\n", &options),
    ///            "<pre><code class=\"language-rust\">fn hello();\n</code></pre>\n");
    ///
    /// options.render.full_info_string = true;
    /// let html = markdown_to_html("``` rust extra info\nfn hello();\n```\n", &options);
    /// let re = regex::Regex::new(r#"data-meta="extra info""#).unwrap();
    /// assert!(re.is_match(&html));
    /// ```
    pub full_info_string: bool,

    /// The wrap column when outputting CommonMark.
    ///
    /// ```
    /// # use comrak::{parse_document, ComrakOptions, format_commonmark};
    /// # fn main() {
    /// # let arena = typed_arena::Arena::new();
    /// let mut options = ComrakOptions::default();
    /// let node = parse_document(&arena, "hello hello hello hello hello hello", &options);
    /// let mut output = vec![];
    /// format_commonmark(node, &options, &mut output).unwrap();
    /// assert_eq!(String::from_utf8(output).unwrap(),
    ///            "hello hello hello hello hello hello\n");
    ///
    /// options.render.width = 20;
    /// let mut output = vec![];
    /// format_commonmark(node, &options, &mut output).unwrap();
    /// assert_eq!(String::from_utf8(output).unwrap(),
    ///            "hello hello hello\nhello hello hello\n");
    /// # }
    /// ```
    pub width: usize,

    /// Allow rendering of raw HTML and potentially dangerous links.
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// let input = "<script>\nalert('xyz');\n</script>\n\n\
    ///              Possibly <marquee>annoying</marquee>.\n\n\
    ///              [Dangerous](javascript:alert(document.cookie)).\n\n\
    ///              [Safe](http://commonmark.org).\n";
    ///
    /// assert_eq!(markdown_to_html(input, &options),
    ///            "<!-- raw HTML omitted -->\n\
    ///             <p>Possibly <!-- raw HTML omitted -->annoying<!-- raw HTML omitted -->.</p>\n\
    ///             <p><a href=\"\">Dangerous</a>.</p>\n\
    ///             <p><a href=\"http://commonmark.org\">Safe</a>.</p>\n");
    ///
    /// options.render.unsafe_ = true;
    /// assert_eq!(markdown_to_html(input, &options),
    ///            "<script>\nalert(\'xyz\');\n</script>\n\
    ///             <p>Possibly <marquee>annoying</marquee>.</p>\n\
    ///             <p><a href=\"javascript:alert(document.cookie)\">Dangerous</a>.</p>\n\
    ///             <p><a href=\"http://commonmark.org\">Safe</a>.</p>\n");
    /// ```
    pub unsafe_: bool,

    /// Escape raw HTML instead of clobbering it.
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// let input = "<i>italic text</i>";
    ///
    /// assert_eq!(markdown_to_html(input, &options),
    ///            "<p><!-- raw HTML omitted -->italic text<!-- raw HTML omitted --></p>\n");
    ///
    /// options.render.escape = true;
    /// assert_eq!(markdown_to_html(input, &options),
    ///            "<p>&lt;i&gt;italic text&lt;/i&gt;</p>\n");
    /// ```
    pub escape: bool,

    /// Set the type of [bullet list marker](https://spec.commonmark.org/0.30/#bullet-list-marker) to use. Options are:
    ///
    /// * `ListStyleType::Dash` to use `-` (default)
    /// * `ListStyleType::Plus` to use `+`
    /// * `ListStyleType::Star` to use `*`
    ///
    /// ```rust
    /// # use comrak::{markdown_to_commonmark, ComrakOptions, ListStyleType};
    /// let mut options = ComrakOptions::default();
    /// let input = "- one\n- two\n- three";
    /// assert_eq!(markdown_to_commonmark(input, &options),
    ///            "- one\n- two\n- three\n"); // default is Dash
    ///
    /// options.render.list_style = ListStyleType::Plus;
    /// assert_eq!(markdown_to_commonmark(input, &options),
    ///            "+ one\n+ two\n+ three\n");
    ///
    /// options.render.list_style = ListStyleType::Star;
    /// assert_eq!(markdown_to_commonmark(input, &options),
    ///            "* one\n* two\n* three\n");
    /// ```
    pub list_style: ListStyleType,
}

#[derive(Default, Debug)]
/// Umbrella plugins struct.
pub struct ComrakPlugins<'p> {
    /// Configure render-time plugins.
    pub render: ComrakRenderPlugins<'p>,
}

#[derive(Default)]
/// Plugins for alternative rendering.
pub struct ComrakRenderPlugins<'p> {
    /// Provide a syntax highlighter adapter implementation for syntax
    /// highlighting of codefence blocks.
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions, ComrakPlugins, markdown_to_html_with_plugins};
    /// # use comrak::adapters::SyntaxHighlighterAdapter;
    /// use std::collections::HashMap;
    /// use std::io::{self, Write};
    /// let options = ComrakOptions::default();
    /// let mut plugins = ComrakPlugins::default();
    /// let input = "```rust\nfn main<'a>();\n```";
    ///
    /// assert_eq!(markdown_to_html_with_plugins(input, &options, &plugins),
    ///            "<pre><code class=\"language-rust\">fn main&lt;'a&gt;();\n</code></pre>\n");
    ///
    /// pub struct MockAdapter {}
    /// impl SyntaxHighlighterAdapter for MockAdapter {
    ///     fn write_highlighted(&self, output: &mut dyn Write, lang: Option<&str>, code: &str) -> io::Result<()> {
    ///         write!(output, "<span class=\"lang-{}\">{}</span>", lang.unwrap(), code)
    ///     }
    ///
    ///     fn write_pre_tag(&self, output: &mut dyn Write, _attributes: HashMap<String, String>) -> io::Result<()> {
    ///         output.write_all(b"<pre lang=\"rust\">")
    ///     }
    ///
    ///     fn write_code_tag(&self, output: &mut dyn Write, _attributes: HashMap<String, String>) -> io::Result<()> {
    ///         output.write_all(b"<code class=\"language-rust\">")
    ///     }
    /// }
    ///
    /// let adapter = MockAdapter {};
    /// plugins.render.codefence_syntax_highlighter = Some(&adapter);
    ///
    /// assert_eq!(markdown_to_html_with_plugins(input, &options, &plugins),
    ///            "<pre lang=\"rust\"><code class=\"language-rust\"><span class=\"lang-rust\">fn main<'a>();\n</span></code></pre>\n");
    /// ```
    pub codefence_syntax_highlighter: Option<&'p dyn SyntaxHighlighterAdapter>,

    /// Optional heading adapter
    pub heading_adapter: Option<&'p dyn HeadingAdapter>,
}

impl Debug for ComrakRenderPlugins<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComrakRenderPlugins")
            .field(
                "codefence_syntax_highlighter",
                &"impl SyntaxHighlighterAdapter",
            )
            .finish()
    }
}

#[derive(Clone)]
pub struct Reference {
    pub url: String,
    pub title: String,
}

struct FootnoteDefinition<'a> {
    ix: Option<u32>,
    node: &'a AstNode<'a>,
}

impl<'a, 'o, 'c> Parser<'a, 'o, 'c> {
    fn new(
        arena: &'a Arena<AstNode<'a>>,
        root: &'a AstNode<'a>,
        options: &'o ComrakOptions,
        callback: Option<Callback<'c>>,
    ) -> Self {
        Parser {
            arena,
            refmap: RefMap::new(),
            root,
            current: root,
            line_number: 0,
            offset: 0,
            column: 0,
            thematic_break_kill_pos: 0,
            first_nonspace: 0,
            first_nonspace_column: 0,
            indent: 0,
            blank: false,
            partially_consumed_tab: false,
            last_line_length: 0,
            last_buffer_ended_with_cr: false,
            total_size: 0,
            options,
            callback,
        }
    }

    fn feed(&mut self, linebuf: &mut Vec<u8>, mut s: &str, eof: bool) {
        if let (0, Some(delimiter)) = (
            self.total_size,
            &self.options.extension.front_matter_delimiter,
        ) {
            if let Some((front_matter, rest)) = split_off_front_matter(s, delimiter) {
                let node =
                    self.add_child(self.root, NodeValue::FrontMatter(front_matter.to_string()));
                s = rest;
                self.finalize(node).unwrap();
            }
        }

        let s = s.as_bytes();

        if s.len() > usize::MAX - self.total_size {
            self.total_size = usize::MAX;
        } else {
            self.total_size += s.len();
        }

        let mut buffer = 0;
        if self.last_buffer_ended_with_cr && !s.is_empty() && s[0] == b'\n' {
            buffer += 1;
        }
        self.last_buffer_ended_with_cr = false;

        let end = s.len();

        while buffer < end {
            let mut process = false;
            let mut eol = buffer;
            while eol < end {
                if strings::is_line_end_char(s[eol]) {
                    process = true;
                    break;
                }
                if s[eol] == 0 {
                    break;
                }
                eol += 1;
            }

            if eol >= end && eof {
                process = true;
            }

            if process {
                if !linebuf.is_empty() {
                    linebuf.extend_from_slice(&s[buffer..eol]);
                    self.process_line(linebuf);
                    linebuf.truncate(0);
                } else {
                    self.process_line(&s[buffer..eol]);
                }
            } else if eol < end && s[eol] == b'\0' {
                linebuf.extend_from_slice(&s[buffer..eol]);
                linebuf.extend_from_slice(&"\u{fffd}".to_string().into_bytes());
            } else {
                linebuf.extend_from_slice(&s[buffer..eol]);
            }

            buffer = eol;
            if buffer < end {
                if s[buffer] == b'\0' {
                    buffer += 1;
                } else {
                    if s[buffer] == b'\r' {
                        buffer += 1;
                        if buffer == end {
                            self.last_buffer_ended_with_cr = true;
                        }
                    }
                    if buffer < end && s[buffer] == b'\n' {
                        buffer += 1;
                    }
                }
            }
        }
    }

    fn scan_thematic_break_inner(&mut self, line: &[u8]) -> (usize, bool) {
        let mut i = self.first_nonspace;

        if i >= line.len() {
            return (i, false);
        }

        let c = line[i];
        if c != b'*' && c != b'_' && c != b'-' {
            return (i, false);
        }

        let mut count = 1;
        let mut nextc;
        loop {
            i += 1;
            if i >= line.len() {
                return (i, false);
            }
            nextc = line[i];

            if nextc == c {
                count += 1;
            } else if nextc != b' ' && nextc != b'\t' {
                break;
            }
        }

        if count >= 3 && (nextc == b'\r' || nextc == b'\n') {
            ((i - self.first_nonspace) + 1, true)
        } else {
            (i, false)
        }
    }

    fn scan_thematic_break(&mut self, line: &[u8]) -> Option<usize> {
        let (offset, found) = self.scan_thematic_break_inner(line);
        if !found {
            self.thematic_break_kill_pos = offset;
            None
        } else {
            Some(offset)
        }
    }

    fn find_first_nonspace(&mut self, line: &[u8]) {
        let mut chars_to_tab = TAB_STOP - (self.column % TAB_STOP);

        if self.first_nonspace <= self.offset {
            self.first_nonspace = self.offset;
            self.first_nonspace_column = self.column;

            loop {
                if self.first_nonspace >= line.len() {
                    break;
                }
                match line[self.first_nonspace] {
                    32 => {
                        self.first_nonspace += 1;
                        self.first_nonspace_column += 1;
                        chars_to_tab -= 1;
                        if chars_to_tab == 0 {
                            chars_to_tab = TAB_STOP;
                        }
                    }
                    9 => {
                        self.first_nonspace += 1;
                        self.first_nonspace_column += chars_to_tab;
                        chars_to_tab = TAB_STOP;
                    }
                    _ => break,
                }
            }
        }

        self.indent = self.first_nonspace_column - self.column;
        self.blank = self.first_nonspace < line.len()
            && strings::is_line_end_char(line[self.first_nonspace]);
    }

    fn process_line(&mut self, line: &[u8]) {
        let mut new_line: Vec<u8>;
        let line = if line.is_empty() || !strings::is_line_end_char(*line.last().unwrap()) {
            new_line = line.into();
            new_line.push(b'\n');
            &new_line
        } else {
            line
        };

        self.offset = 0;
        self.column = 0;
        self.first_nonspace = 0;
        self.first_nonspace_column = 0;
        self.indent = 0;
        self.thematic_break_kill_pos = 0;
        self.blank = false;
        self.partially_consumed_tab = false;

        if self.line_number == 0
            && line.len() >= 3
            && unsafe { str::from_utf8_unchecked(line) }.starts_with('\u{feff}')
        {
            self.offset += 3;
        }

        self.line_number += 1;

        let mut all_matched = true;
        if let Some(last_matched_container) = self.check_open_blocks(line, &mut all_matched) {
            let mut container = last_matched_container;
            let current = self.current;
            self.open_new_blocks(&mut container, line, all_matched);

            if current.same_node(self.current) {
                self.add_text_to_container(container, last_matched_container, line);
            }
        }

        self.last_line_length = line.len();
        if self.last_line_length > 0 && line[self.last_line_length - 1] == b'\n' {
            self.last_line_length -= 1;
        }
        if self.last_line_length > 0 && line[self.last_line_length - 1] == b'\r' {
            self.last_line_length -= 1;
        }
    }

    fn check_open_blocks(
        &mut self,
        line: &[u8],
        all_matched: &mut bool,
    ) -> Option<&'a AstNode<'a>> {
        let (new_all_matched, mut container, should_continue) =
            self.check_open_blocks_inner(self.root, line);

        *all_matched = new_all_matched;
        if !*all_matched {
            container = container.parent().unwrap();
        }

        if !should_continue {
            None
        } else {
            Some(container)
        }
    }

    fn check_open_blocks_inner(
        &mut self,
        mut container: &'a AstNode<'a>,
        line: &[u8],
    ) -> (bool, &'a AstNode<'a>, bool) {
        let mut should_continue = true;

        while nodes::last_child_is_open(container) {
            container = container.last_child().unwrap();
            let ast = &mut *container.data.borrow_mut();

            self.find_first_nonspace(line);

            match ast.value {
                NodeValue::BlockQuote => {
                    if !self.parse_block_quote_prefix(line) {
                        return (false, container, should_continue);
                    }
                }
                NodeValue::Item(ref nl) => {
                    if !self.parse_node_item_prefix(line, container, nl) {
                        return (false, container, should_continue);
                    }
                }
                NodeValue::DescriptionItem(ref di) => {
                    if !self.parse_description_item_prefix(line, container, di) {
                        return (false, container, should_continue);
                    }
                }
                NodeValue::CodeBlock(..) => {
                    if !self.parse_code_block_prefix(line, container, ast, &mut should_continue) {
                        return (false, container, should_continue);
                    }
                }
                NodeValue::HtmlBlock(ref nhb) => {
                    if !self.parse_html_block_prefix(nhb.block_type) {
                        return (false, container, should_continue);
                    }
                }
                NodeValue::Paragraph => {
                    if self.blank {
                        return (false, container, should_continue);
                    }
                }
                NodeValue::Table(..) => {
                    if !table::matches(&line[self.first_nonspace..]) {
                        return (false, container, should_continue);
                    }
                    continue;
                }
                NodeValue::Heading(..) | NodeValue::TableRow(..) | NodeValue::TableCell => {
                    return (false, container, should_continue);
                }
                NodeValue::FootnoteDefinition(..) => {
                    if !self.parse_footnote_definition_block_prefix(line) {
                        return (false, container, should_continue);
                    }
                }
                _ => {}
            }
        }

        (true, container, should_continue)
    }

    fn open_new_blocks(&mut self, container: &mut &'a AstNode<'a>, line: &[u8], all_matched: bool) {
        let mut matched: usize = 0;
        let mut nl: NodeList = NodeList::default();
        let mut sc: scanners::SetextChar = scanners::SetextChar::Equals;
        let mut maybe_lazy = node_matches!(self.current, NodeValue::Paragraph);

        while !node_matches!(
            container,
            NodeValue::CodeBlock(..) | NodeValue::HtmlBlock(..)
        ) {
            self.find_first_nonspace(line);
            let indented = self.indent >= CODE_INDENT;

            if !indented && line[self.first_nonspace] == b'>' {
                let offset = self.first_nonspace + 1 - self.offset;
                self.advance_offset(line, offset, false);
                if strings::is_space_or_tab(line[self.offset]) {
                    self.advance_offset(line, 1, true);
                }
                *container = self.add_child(container, NodeValue::BlockQuote);
            } else if !indented
                && unwrap_into(
                    scanners::atx_heading_start(&line[self.first_nonspace..]),
                    &mut matched,
                )
            {
                let heading_startpos = self.first_nonspace;
                let offset = self.offset;
                self.advance_offset(line, heading_startpos + matched - offset, false);
                *container = self.add_child(container, NodeValue::Heading(NodeHeading::default()));

                let mut hashpos = line[self.first_nonspace..]
                    .iter()
                    .position(|&c| c == b'#')
                    .unwrap()
                    + self.first_nonspace;
                let mut level = 0;
                while line[hashpos] == b'#' {
                    level += 1;
                    hashpos += 1;
                }

                container.data.borrow_mut().value = NodeValue::Heading(NodeHeading {
                    level,
                    setext: false,
                });
            } else if !indented
                && unwrap_into(
                    scanners::open_code_fence(&line[self.first_nonspace..]),
                    &mut matched,
                )
            {
                let first_nonspace = self.first_nonspace;
                let offset = self.offset;
                let ncb = NodeCodeBlock {
                    fenced: true,
                    fence_char: line[first_nonspace],
                    fence_length: matched,
                    fence_offset: first_nonspace - offset,
                    info: String::with_capacity(10),
                    literal: String::new(),
                };
                *container = self.add_child(container, NodeValue::CodeBlock(ncb));
                self.advance_offset(line, first_nonspace + matched - offset, false);
            } else if !indented
                && (unwrap_into(
                    scanners::html_block_start(&line[self.first_nonspace..]),
                    &mut matched,
                ) || (!node_matches!(container, NodeValue::Paragraph)
                    && unwrap_into(
                        scanners::html_block_start_7(&line[self.first_nonspace..]),
                        &mut matched,
                    )))
            {
                let nhb = NodeHtmlBlock {
                    block_type: matched as u8,
                    literal: String::new(),
                };

                *container = self.add_child(container, NodeValue::HtmlBlock(nhb));
            } else if !indented
                && node_matches!(container, NodeValue::Paragraph)
                && unwrap_into(
                    scanners::setext_heading_line(&line[self.first_nonspace..]),
                    &mut sc,
                )
            {
                let has_content = {
                    let mut ast = container.data.borrow_mut();
                    self.resolve_reference_link_definitions(&mut ast.content)
                };
                if has_content {
                    container.data.borrow_mut().value = NodeValue::Heading(NodeHeading {
                        level: match sc {
                            scanners::SetextChar::Equals => 1,
                            scanners::SetextChar::Hyphen => 2,
                        },
                        setext: true,
                    });
                    let adv = line.len() - 1 - self.offset;
                    self.advance_offset(line, adv, false);
                }
            } else if !indented
                && !matches!(
                    (&container.data.borrow().value, all_matched),
                    (&NodeValue::Paragraph, false)
                )
                && self.thematic_break_kill_pos <= self.first_nonspace
                && unwrap_into(self.scan_thematic_break(line), &mut matched)
            {
                *container = self.add_child(container, NodeValue::ThematicBreak);
                let adv = line.len() - 1 - self.offset;
                self.advance_offset(line, adv, false);
            } else if !indented
                && self.options.extension.footnotes
                && unwrap_into(
                    scanners::footnote_definition(&line[self.first_nonspace..]),
                    &mut matched,
                )
            {
                let mut c = &line[self.first_nonspace + 2..self.first_nonspace + matched];
                c = c.split(|&e| e == b']').next().unwrap();
                let offset = self.first_nonspace + matched - self.offset;
                self.advance_offset(line, offset, false);
                *container = self.add_child(
                    container,
                    NodeValue::FootnoteDefinition(str::from_utf8(c).unwrap().to_string()),
                );
            } else if !indented
                && self.options.extension.description_lists
                && line[self.first_nonspace] == b':'
                && self.parse_desc_list_details(container)
            {
                let offset = self.first_nonspace + 1 - self.offset;
                self.advance_offset(line, offset, false);
                if strings::is_space_or_tab(line[self.offset]) {
                    self.advance_offset(line, 1, true);
                }
            } else if (!indented || node_matches!(container, NodeValue::List(..)))
                && self.indent < 4
                && unwrap_into_2(
                    parse_list_marker(
                        line,
                        self.first_nonspace,
                        node_matches!(container, NodeValue::Paragraph),
                    ),
                    &mut matched,
                    &mut nl,
                )
            {
                let offset = self.first_nonspace + matched - self.offset;
                self.advance_offset(line, offset, false);
                let (save_partially_consumed_tab, save_offset, save_column) =
                    (self.partially_consumed_tab, self.offset, self.column);

                while self.column - save_column <= 5 && strings::is_space_or_tab(line[self.offset])
                {
                    self.advance_offset(line, 1, true);
                }

                let i = self.column - save_column;
                if !(1..5).contains(&i) || strings::is_line_end_char(line[self.offset]) {
                    nl.padding = matched + 1;
                    self.offset = save_offset;
                    self.column = save_column;
                    self.partially_consumed_tab = save_partially_consumed_tab;
                    if i > 0 {
                        self.advance_offset(line, 1, true);
                    }
                } else {
                    nl.padding = matched + i;
                }

                nl.marker_offset = self.indent;

                if match container.data.borrow().value {
                    NodeValue::List(ref mnl) => !lists_match(&nl, mnl),
                    _ => true,
                } {
                    *container = self.add_child(container, NodeValue::List(nl));
                }

                *container = self.add_child(container, NodeValue::Item(nl));
            } else if indented && !maybe_lazy && !self.blank {
                self.advance_offset(line, CODE_INDENT, true);
                let ncb = NodeCodeBlock {
                    fenced: false,
                    fence_char: 0,
                    fence_length: 0,
                    fence_offset: 0,
                    info: String::new(),
                    literal: String::new(),
                };
                *container = self.add_child(container, NodeValue::CodeBlock(ncb));
            } else {
                let new_container = if !indented && self.options.extension.table {
                    table::try_opening_block(self, container, line)
                } else {
                    None
                };

                match new_container {
                    Some((new_container, replace, mark_visited)) => {
                        if replace {
                            container.insert_after(new_container);
                            container.detach();
                            *container = new_container;
                        } else {
                            *container = new_container;
                        }
                        if mark_visited {
                            container.data.borrow_mut().table_visited = true;
                        }
                    }
                    _ => break,
                }
            }

            if container.data.borrow().value.accepts_lines() {
                break;
            }

            maybe_lazy = false;
        }
    }

    fn advance_offset(&mut self, line: &[u8], mut count: usize, columns: bool) {
        while count > 0 {
            match line[self.offset] {
                9 => {
                    let chars_to_tab = TAB_STOP - (self.column % TAB_STOP);
                    if columns {
                        self.partially_consumed_tab = chars_to_tab > count;
                        let chars_to_advance = min(count, chars_to_tab);
                        self.column += chars_to_advance;
                        self.offset += if self.partially_consumed_tab { 0 } else { 1 };
                        count -= chars_to_advance;
                    } else {
                        self.partially_consumed_tab = false;
                        self.column += chars_to_tab;
                        self.offset += 1;
                        count -= 1;
                    }
                }
                _ => {
                    self.partially_consumed_tab = false;
                    self.offset += 1;
                    self.column += 1;
                    count -= 1;
                }
            }
        }
    }

    fn parse_block_quote_prefix(&mut self, line: &[u8]) -> bool {
        let indent = self.indent;
        if indent <= 3 && line[self.first_nonspace] == b'>' {
            self.advance_offset(line, indent + 1, true);

            if strings::is_space_or_tab(line[self.offset]) {
                self.advance_offset(line, 1, true);
            }

            return true;
        }

        false
    }

    fn parse_footnote_definition_block_prefix(&mut self, line: &[u8]) -> bool {
        if self.indent >= 4 {
            self.advance_offset(line, 4, true);
            true
        } else {
            line == b"\n" || line == b"\r\n"
        }
    }

    fn parse_node_item_prefix(
        &mut self,
        line: &[u8],
        container: &'a AstNode<'a>,
        nl: &NodeList,
    ) -> bool {
        if self.indent >= nl.marker_offset + nl.padding {
            self.advance_offset(line, nl.marker_offset + nl.padding, true);
            true
        } else if self.blank && container.first_child().is_some() {
            let offset = self.first_nonspace - self.offset;
            self.advance_offset(line, offset, false);
            true
        } else {
            false
        }
    }

    fn parse_description_item_prefix(
        &mut self,
        line: &[u8],
        container: &'a AstNode<'a>,
        di: &NodeDescriptionItem,
    ) -> bool {
        if self.indent >= di.marker_offset + di.padding {
            self.advance_offset(line, di.marker_offset + di.padding, true);
            true
        } else if self.blank && container.first_child().is_some() {
            let offset = self.first_nonspace - self.offset;
            self.advance_offset(line, offset, false);
            true
        } else {
            false
        }
    }

    fn parse_code_block_prefix(
        &mut self,
        line: &[u8],
        container: &'a AstNode<'a>,
        ast: &mut Ast,
        should_continue: &mut bool,
    ) -> bool {
        let (fenced, fence_char, fence_length, fence_offset) = match ast.value {
            NodeValue::CodeBlock(ref ncb) => (
                ncb.fenced,
                ncb.fence_char,
                ncb.fence_length,
                ncb.fence_offset,
            ),
            _ => unreachable!(),
        };

        if !fenced {
            if self.indent >= CODE_INDENT {
                self.advance_offset(line, CODE_INDENT, true);
                return true;
            } else if self.blank {
                let offset = self.first_nonspace - self.offset;
                self.advance_offset(line, offset, false);
                return true;
            }
            return false;
        }

        let matched = if self.indent <= 3 && line[self.first_nonspace] == fence_char {
            scanners::close_code_fence(&line[self.first_nonspace..]).unwrap_or(0)
        } else {
            0
        };

        if matched >= fence_length {
            *should_continue = false;
            self.advance_offset(line, matched, false);
            self.current = self.finalize_borrowed(container, ast).unwrap();
            return false;
        }

        let mut i = fence_offset;
        while i > 0 && strings::is_space_or_tab(line[self.offset]) {
            self.advance_offset(line, 1, true);
            i -= 1;
        }
        true
    }

    fn parse_html_block_prefix(&mut self, t: u8) -> bool {
        match t {
            1 | 2 | 3 | 4 | 5 => true,
            6 | 7 => !self.blank,
            _ => unreachable!(),
        }
    }

    fn parse_desc_list_details(&mut self, container: &mut &'a AstNode<'a>) -> bool {
        let last_child = match container.last_child() {
            Some(lc) => lc,
            None => return false,
        };

        if node_matches!(last_child, NodeValue::Paragraph) {
            // We have found the details after the paragraph for the term.
            //
            // This paragraph is moved as a child of a new DescriptionTerm node.
            //
            // If the node before the paragraph is a description list, the item
            // is added to it. If not, create a new list.

            last_child.detach();

            let list = match container.last_child() {
                Some(lc) if node_matches!(lc, NodeValue::DescriptionList) => {
                    reopen_ast_nodes(lc);
                    lc
                }
                _ => self.add_child(container, NodeValue::DescriptionList),
            };

            let metadata = NodeDescriptionItem {
                marker_offset: self.indent,
                padding: 2,
            };

            let item = self.add_child(list, NodeValue::DescriptionItem(metadata));
            let term = self.add_child(item, NodeValue::DescriptionTerm);
            let details = self.add_child(item, NodeValue::DescriptionDetails);

            term.append(last_child);

            *container = details;

            true
        } else {
            false
        }
    }

    fn add_child(&mut self, mut parent: &'a AstNode<'a>, value: NodeValue) -> &'a AstNode<'a> {
        while !nodes::can_contain_type(parent, &value) {
            parent = self.finalize(parent).unwrap();
        }

        let mut child = Ast::new(value);
        child.start_line = self.line_number;
        let node = self.arena.alloc(Node::new(RefCell::new(child)));
        parent.append(node);
        node
    }

    fn add_text_to_container(
        &mut self,
        mut container: &'a AstNode<'a>,
        last_matched_container: &'a AstNode<'a>,
        line: &[u8],
    ) {
        self.find_first_nonspace(line);

        if self.blank {
            if let Some(last_child) = container.last_child() {
                last_child.data.borrow_mut().last_line_blank = true;
            }
        }

        container.data.borrow_mut().last_line_blank = self.blank
            && match container.data.borrow().value {
                NodeValue::BlockQuote | NodeValue::Heading(..) | NodeValue::ThematicBreak => false,
                NodeValue::CodeBlock(ref ncb) => !ncb.fenced,
                NodeValue::Item(..) => {
                    container.first_child().is_some()
                        || container.data.borrow().start_line != self.line_number
                }
                _ => true,
            };

        let mut tmp = container;
        while let Some(parent) = tmp.parent() {
            parent.data.borrow_mut().last_line_blank = false;
            tmp = parent;
        }

        if !self.current.same_node(last_matched_container)
            && container.same_node(last_matched_container)
            && !self.blank
            && node_matches!(self.current, NodeValue::Paragraph)
        {
            self.add_line(self.current, line);
        } else {
            while !self.current.same_node(last_matched_container) {
                self.current = self.finalize(self.current).unwrap();
            }

            let add_text_result = match container.data.borrow().value {
                NodeValue::CodeBlock(..) => AddTextResult::CodeBlock,
                NodeValue::HtmlBlock(ref nhb) => AddTextResult::HtmlBlock(nhb.block_type),
                _ => AddTextResult::Otherwise,
            };

            match add_text_result {
                AddTextResult::CodeBlock => {
                    self.add_line(container, line);
                }
                AddTextResult::HtmlBlock(block_type) => {
                    self.add_line(container, line);

                    let matches_end_condition = match block_type {
                        1 => scanners::html_block_end_1(&line[self.first_nonspace..]),
                        2 => scanners::html_block_end_2(&line[self.first_nonspace..]),
                        3 => scanners::html_block_end_3(&line[self.first_nonspace..]),
                        4 => scanners::html_block_end_4(&line[self.first_nonspace..]),
                        5 => scanners::html_block_end_5(&line[self.first_nonspace..]),
                        _ => false,
                    };

                    if matches_end_condition {
                        container = self.finalize(container).unwrap();
                    }
                }
                _ => {
                    if self.blank {
                        // do nothing
                    } else if container.data.borrow().value.accepts_lines() {
                        let mut line: Vec<u8> = line.into();
                        if let NodeValue::Heading(ref nh) = container.data.borrow().value {
                            if !nh.setext {
                                strings::chop_trailing_hashtags(&mut line);
                            }
                        };
                        let count = self.first_nonspace - self.offset;

                        // In a rare case the above `chop` operation can leave
                        // the line shorter than the recorded `first_nonspace`
                        // This happens with ATX headers containing no header
                        // text, multiple spaces and trailing hashes, e.g
                        //
                        // ###     ###
                        //
                        // In this case `first_nonspace` indexes into the second
                        // set of hashes, while `chop_trailing_hashtags` truncates
                        // `line` to just `###` (the first three hashes).
                        // In this case there's no text to add, and no further
                        // processing to be done.
                        let have_line_text = self.first_nonspace <= line.len();

                        if have_line_text {
                            self.advance_offset(&line, count, false);
                            self.add_line(container, &line);
                        }
                    } else {
                        container = self.add_child(container, NodeValue::Paragraph);
                        let count = self.first_nonspace - self.offset;
                        self.advance_offset(line, count, false);
                        self.add_line(container, line);
                    }
                }
            }

            self.current = container;
        }
    }

    fn add_line(&mut self, node: &'a AstNode<'a>, line: &[u8]) {
        let mut ast = node.data.borrow_mut();
        assert!(ast.open);
        if self.partially_consumed_tab {
            self.offset += 1;
            let chars_to_tab = TAB_STOP - (self.column % TAB_STOP);
            for _ in 0..chars_to_tab {
                ast.content.push(' ');
            }
        }
        if self.offset < line.len() {
            ast.content
                .push_str(str::from_utf8(&line[self.offset..]).unwrap()); // TODO: try propagating &[u8] to &str up from here
        }
    }

    fn finish(&mut self, remaining: Vec<u8>) -> &'a AstNode<'a> {
        if !remaining.is_empty() {
            self.process_line(&remaining);
        }

        self.finalize_document();
        self.postprocess_text_nodes(self.root);
        self.root
    }

    fn finalize_document(&mut self) {
        while !self.current.same_node(self.root) {
            self.current = self.finalize(self.current).unwrap();
        }

        self.finalize(self.root);

        self.refmap.max_ref_size = if self.total_size > 100000 {
            self.total_size
        } else {
            100000
        };

        self.process_inlines();
        if self.options.extension.footnotes {
            self.process_footnotes();
        }
    }

    fn finalize(&mut self, node: &'a AstNode<'a>) -> Option<&'a AstNode<'a>> {
        self.finalize_borrowed(node, &mut node.data.borrow_mut())
    }

    fn resolve_reference_link_definitions(&mut self, content: &mut String) -> bool {
        let mut seeked = 0;
        {
            let mut pos = 0;
            let mut seek: &[u8] = content.as_bytes();
            while !seek.is_empty()
                && seek[0] == b'['
                && unwrap_into(self.parse_reference_inline(seek), &mut pos)
            {
                seek = &seek[pos..];
                seeked += pos;
            }
        }

        if seeked != 0 {
            *content = content[seeked..].to_string();
        }

        !strings::is_blank(content.as_bytes())
    }

    fn finalize_borrowed(
        &mut self,
        node: &'a AstNode<'a>,
        ast: &mut Ast,
    ) -> Option<&'a AstNode<'a>> {
        assert!(ast.open);
        ast.open = false;

        let content = &mut ast.content;
        let parent = node.parent();

        match ast.value {
            NodeValue::Paragraph => {
                let has_content = self.resolve_reference_link_definitions(content);
                if !has_content {
                    node.detach();
                }
            }
            NodeValue::CodeBlock(ref mut ncb) => {
                if !ncb.fenced {
                    strings::remove_trailing_blank_lines(content);
                    content.push('\n');
                } else {
                    let mut pos = 0;
                    while pos < content.len() {
                        if strings::is_line_end_char(content.as_bytes()[pos]) {
                            break;
                        }
                        pos += 1;
                    }
                    assert!(pos < content.len());

                    let mut tmp = entity::unescape_html(&content.as_bytes()[..pos]);
                    strings::trim(&mut tmp);
                    strings::unescape(&mut tmp);
                    if tmp.is_empty() {
                        ncb.info = self
                            .options
                            .parse
                            .default_info_string
                            .as_ref()
                            .map_or(String::new(), |s| s.clone());
                    } else {
                        ncb.info = String::from_utf8(tmp).unwrap();
                    }

                    if content.as_bytes()[pos] == b'\r' {
                        pos += 1;
                    }
                    if content.as_bytes()[pos] == b'\n' {
                        pos += 1;
                    }

                    content.drain(..pos);
                }
                mem::swap(&mut ncb.literal, content);
            }
            NodeValue::HtmlBlock(ref mut nhb) => {
                mem::swap(&mut nhb.literal, content);
            }
            NodeValue::List(ref mut nl) => {
                nl.tight = true;
                let mut ch = node.first_child();

                while let Some(item) = ch {
                    if item.data.borrow().last_line_blank && item.next_sibling().is_some() {
                        nl.tight = false;
                        break;
                    }

                    let mut subch = item.first_child();
                    while let Some(subitem) = subch {
                        if (item.next_sibling().is_some() || subitem.next_sibling().is_some())
                            && nodes::ends_with_blank_line(subitem)
                        {
                            nl.tight = false;
                            break;
                        }
                        subch = subitem.next_sibling();
                    }

                    if !nl.tight {
                        break;
                    }

                    ch = item.next_sibling();
                }
            }
            _ => (),
        }

        parent
    }

    fn process_inlines(&mut self) {
        self.process_inlines_node(self.root);
    }

    fn process_inlines_node(&mut self, node: &'a AstNode<'a>) {
        for node in node.descendants() {
            if node.data.borrow().value.contains_inlines() {
                self.parse_inlines(node);
            }
        }
    }

    fn parse_inlines(&mut self, node: &'a AstNode<'a>) {
        let delimiter_arena = Arena::new();
        let node_data = node.data.borrow();
        let content = strings::rtrim_slice(node_data.content.as_bytes());
        let mut subj = inlines::Subject::new(
            self.arena,
            self.options,
            content,
            &mut self.refmap,
            &delimiter_arena,
            self.callback.as_mut(),
        );

        while subj.parse_inline(node) {}

        subj.process_emphasis(0);

        while subj.pop_bracket() {}
    }

    fn process_footnotes(&mut self) {
        let mut map = HashMap::new();
        Self::find_footnote_definitions(self.root, &mut map);

        let mut ix = 0;
        Self::find_footnote_references(self.root, &mut map, &mut ix);

        if ix > 0 {
            let mut v = map.into_values().collect::<Vec<_>>();
            v.sort_unstable_by(|a, b| a.ix.cmp(&b.ix));
            for f in v {
                if let Some(ix) = f.ix {
                    match f.node.data.borrow_mut().value {
                        NodeValue::FootnoteDefinition(ref mut name) => {
                            *name = format!("{}", ix);
                        }
                        _ => unreachable!(),
                    }
                    self.root.append(f.node);
                }
            }
        }
    }

    fn find_footnote_definitions(
        node: &'a AstNode<'a>,
        map: &mut HashMap<String, FootnoteDefinition<'a>>,
    ) {
        match node.data.borrow().value {
            NodeValue::FootnoteDefinition(ref name) => {
                node.detach();
                map.insert(
                    strings::normalize_label(name),
                    FootnoteDefinition { ix: None, node },
                );
            }
            _ => {
                for n in node.children() {
                    Self::find_footnote_definitions(n, map);
                }
            }
        }
    }

    fn find_footnote_references(
        node: &'a AstNode<'a>,
        map: &mut HashMap<String, FootnoteDefinition>,
        ixp: &mut u32,
    ) {
        let mut ast = node.data.borrow_mut();
        let mut replace = None;
        match ast.value {
            NodeValue::FootnoteReference(ref mut name) => {
                if let Some(ref mut footnote) = map.get_mut(name) {
                    let ix = match footnote.ix {
                        Some(ix) => ix,
                        None => {
                            *ixp += 1;
                            footnote.ix = Some(*ixp);
                            *ixp
                        }
                    };
                    *name = format!("{}", ix);
                } else {
                    replace = Some(name.clone());
                }
            }
            _ => {
                for n in node.children() {
                    Self::find_footnote_references(n, map, ixp);
                }
            }
        }

        if let Some(mut label) = replace {
            label.insert_str(0, "[^");
            label.push(']');
            ast.value = NodeValue::Text(label);
        }
    }

    fn postprocess_text_nodes(&mut self, node: &'a AstNode<'a>) {
        let mut stack = vec![node];
        let mut children = vec![];

        while let Some(node) = stack.pop() {
            let mut nch = node.first_child();

            while let Some(n) = nch {
                let mut this_bracket = false;
                loop {
                    match n.data.borrow_mut().value {
                        // Join adjacent text nodes together
                        NodeValue::Text(ref mut root) => {
                            let ns = match n.next_sibling() {
                                Some(ns) => ns,
                                _ => {
                                    // Post-process once we are finished joining text nodes
                                    self.postprocess_text_node(n, root);
                                    break;
                                }
                            };

                            match ns.data.borrow().value {
                                NodeValue::Text(ref adj) => {
                                    root.push_str(adj);
                                    ns.detach();
                                }
                                _ => {
                                    // Post-process once we are finished joining text nodes
                                    self.postprocess_text_node(n, root);
                                    break;
                                }
                            }
                        }
                        NodeValue::Link(..) | NodeValue::Image(..) => {
                            this_bracket = true;
                            break;
                        }
                        _ => break,
                    }
                }

                if !this_bracket {
                    children.push(n);
                }

                nch = n.next_sibling();
            }

            // Push children onto work stack in reverse order so they are
            // traversed in order
            stack.extend(children.drain(..).rev());
        }
    }

    fn postprocess_text_node(&mut self, node: &'a AstNode<'a>, text: &mut String) {
        if self.options.extension.tasklist {
            self.process_tasklist(node, text);
        }

        if self.options.extension.autolink {
            autolink::process_autolinks(self.arena, node, text);
        }
    }

    fn process_tasklist(&mut self, node: &'a AstNode<'a>, text: &mut String) {
        let (end, symbol) = match scanners::tasklist(text.as_bytes()) {
            Some(p) => p,
            None => return,
        };

        let symbol = symbol as char;

        if !self.options.parse.relaxed_tasklist_matching && !matches!(symbol, ' ' | 'x' | 'X') {
            return;
        }

        let parent = node.parent().unwrap();
        if node.previous_sibling().is_some() || parent.previous_sibling().is_some() {
            return;
        }

        if !node_matches!(parent, NodeValue::Paragraph) {
            return;
        }

        if !node_matches!(parent.parent().unwrap(), NodeValue::Item(..)) {
            return;
        }

        text.drain(..end);

        let checkbox = inlines::make_inline(
            self.arena,
            NodeValue::TaskItem {
                symbol: if symbol == ' ' { None } else { Some(symbol) },
            },
        );
        node.insert_before(checkbox);
    }

    fn parse_reference_inline(&mut self, content: &[u8]) -> Option<usize> {
        // In this case reference inlines rarely have delimiters
        // so we often just need the minimal case
        let delimiter_arena = Arena::with_capacity(0);
        let mut subj = inlines::Subject::new(
            self.arena,
            self.options,
            content,
            &mut self.refmap,
            &delimiter_arena,
            self.callback.as_mut(),
        );

        let mut lab: String = match subj.link_label() {
            Some(lab) if !lab.is_empty() => lab.to_string(),
            _ => return None,
        };

        if subj.peek_char() != Some(&(b':')) {
            return None;
        }

        subj.pos += 1;
        subj.spnl();
        let (url, matchlen) = match inlines::manual_scan_link_url(&subj.input[subj.pos..]) {
            Some((url, matchlen)) => (url, matchlen),
            None => return None,
        };
        subj.pos += matchlen;

        let beforetitle = subj.pos;
        subj.spnl();
        let title_search = if subj.pos == beforetitle {
            None
        } else {
            scanners::link_title(&subj.input[subj.pos..])
        };
        let title = match title_search {
            Some(matchlen) => {
                let t = &subj.input[subj.pos..subj.pos + matchlen];
                subj.pos += matchlen;
                t.to_vec()
            }
            _ => {
                subj.pos = beforetitle;
                vec![]
            }
        };

        subj.skip_spaces();
        if !subj.skip_line_end() {
            if !title.is_empty() {
                subj.pos = beforetitle;
                subj.skip_spaces();
                if !subj.skip_line_end() {
                    return None;
                }
            } else {
                return None;
            }
        }

        lab = strings::normalize_label(&lab);
        if !lab.is_empty() {
            subj.refmap.map.entry(lab).or_insert(Reference {
                url: String::from_utf8(strings::clean_url(url)).unwrap(),
                title: String::from_utf8(strings::clean_title(&title)).unwrap(),
            });
        }
        Some(subj.pos)
    }
}

enum AddTextResult {
    CodeBlock,
    HtmlBlock(u8),
    Otherwise,
}

fn parse_list_marker(
    line: &[u8],
    mut pos: usize,
    interrupts_paragraph: bool,
) -> Option<(usize, NodeList)> {
    let mut c = line[pos];
    let startpos = pos;

    if c == b'*' || c == b'-' || c == b'+' {
        pos += 1;
        if !isspace(line[pos]) {
            return None;
        }

        if interrupts_paragraph {
            let mut i = pos;
            while strings::is_space_or_tab(line[i]) {
                i += 1;
            }
            if line[i] == b'\n' {
                return None;
            }
        }

        return Some((
            pos - startpos,
            NodeList {
                list_type: ListType::Bullet,
                marker_offset: 0,
                padding: 0,
                start: 1,
                delimiter: ListDelimType::Period,
                bullet_char: c,
                tight: false,
            },
        ));
    } else if isdigit(c) {
        let mut start: usize = 0;
        let mut digits = 0;

        loop {
            start = (10 * start) + (line[pos] - b'0') as usize;
            pos += 1;
            digits += 1;

            if !(digits < 9 && isdigit(line[pos])) {
                break;
            }
        }

        if interrupts_paragraph && start != 1 {
            return None;
        }

        c = line[pos];
        if c != b'.' && c != b')' {
            return None;
        }

        pos += 1;

        if !isspace(line[pos]) {
            return None;
        }

        if interrupts_paragraph {
            let mut i = pos;
            while strings::is_space_or_tab(line[i]) {
                i += 1;
            }
            if strings::is_line_end_char(line[i]) {
                return None;
            }
        }

        return Some((
            pos - startpos,
            NodeList {
                list_type: ListType::Ordered,
                marker_offset: 0,
                padding: 0,
                start,
                delimiter: if c == b'.' {
                    ListDelimType::Period
                } else {
                    ListDelimType::Paren
                },
                bullet_char: 0,
                tight: false,
            },
        ));
    }

    None
}

pub fn unwrap_into<T>(t: Option<T>, out: &mut T) -> bool {
    match t {
        Some(v) => {
            *out = v;
            true
        }
        _ => false,
    }
}

pub fn unwrap_into_copy<T: Copy>(t: Option<&T>, out: &mut T) -> bool {
    match t {
        Some(v) => {
            *out = *v;
            true
        }
        _ => false,
    }
}

fn unwrap_into_2<T, U>(tu: Option<(T, U)>, out_t: &mut T, out_u: &mut U) -> bool {
    match tu {
        Some((t, u)) => {
            *out_t = t;
            *out_u = u;
            true
        }
        _ => false,
    }
}

fn lists_match(list_data: &NodeList, item_data: &NodeList) -> bool {
    list_data.list_type == item_data.list_type
        && list_data.delimiter == item_data.delimiter
        && list_data.bullet_char == item_data.bullet_char
}

fn reopen_ast_nodes<'a>(mut ast: &'a AstNode<'a>) {
    loop {
        ast.data.borrow_mut().open = true;
        ast = match ast.parent() {
            Some(p) => p,
            None => return,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutolinkType {
    Uri,
    Email,
}

#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
/// Options for bulleted list redering in markdown. See `link_style` in [ComrakRenderOptions] for more details.
pub enum ListStyleType {
    /// The `-` character
    #[default]
    Dash = 45,
    /// The `+` character
    Plus = 43,
    /// The `*` character
    Star = 42,
}
