use crate::{
    compositor::{Component, Compositor, Context, Event, EventResult},
    ctrl, key, shift,
    ui::{self, fuzzy_match::FuzzyQuery, EditorView},
};
use futures_util::future::BoxFuture;
use tui::{
    buffer::Buffer as Surface,
    widgets::{Block, BorderType, Borders},
};

use fuzzy_matcher::skim::SkimMatcherV2 as Matcher;
use tui::widgets::Widget;

use std::{cmp::Ordering, time::Instant};
use std::{collections::HashMap, io::Read, path::PathBuf};

use crate::ui::{Prompt, PromptEvent};
use helix_core::{movement::Direction, Position};
use helix_view::{
    editor::Action,
    graphics::{CursorKind, Margin, Modifier, Rect},
    Document, DocumentId, Editor,
};

use super::{menu::Item, overlay::Overlay};

pub const MIN_AREA_WIDTH_FOR_PREVIEW: u16 = 72;
/// Biggest file size to preview in bytes
pub const MAX_FILE_SIZE_FOR_PREVIEW: u64 = 10 * 1024 * 1024;

#[derive(PartialEq, Eq, Hash)]
pub enum PathOrId {
    Id(DocumentId),
    Path(PathBuf),
}

impl PathOrId {
    fn get_canonicalized(self) -> std::io::Result<Self> {
        use PathOrId::*;
        Ok(match self {
            Path(path) => Path(helix_core::path::get_canonicalized_path(&path)?),
            Id(id) => Id(id),
        })
    }
}

impl From<PathBuf> for PathOrId {
    fn from(v: PathBuf) -> Self {
        Self::Path(v)
    }
}

impl From<DocumentId> for PathOrId {
    fn from(v: DocumentId) -> Self {
        Self::Id(v)
    }
}

/// File path and range of lines (used to align and highlight lines)
pub type FileLocation = (PathOrId, Option<(usize, usize)>);

pub struct FilePicker<T: Item> {
    picker: Picker<T>,
    pub truncate_start: bool,
    /// Caches paths to documents
    preview_cache: HashMap<PathBuf, CachedPreview>,
    read_buffer: Vec<u8>,
    /// Given an item in the picker, return the file path and line number to display.
    file_fn: Box<dyn Fn(&Editor, &T) -> Option<FileLocation>>,
}

pub enum CachedPreview {
    Document(Box<Document>),
    Binary,
    LargeFile,
    NotFound,
}

// We don't store this enum in the cache so as to avoid lifetime constraints
// from borrowing a document already opened in the editor.
pub enum Preview<'picker, 'editor> {
    Cached(&'picker CachedPreview),
    EditorDocument(&'editor Document),
}

impl Preview<'_, '_> {
    fn document(&self) -> Option<&Document> {
        match self {
            Preview::EditorDocument(doc) => Some(doc),
            Preview::Cached(CachedPreview::Document(doc)) => Some(doc),
            _ => None,
        }
    }

    /// Alternate text to show for the preview.
    fn placeholder(&self) -> &str {
        match *self {
            Self::EditorDocument(_) => "<File preview>",
            Self::Cached(preview) => match preview {
                CachedPreview::Document(_) => "<File preview>",
                CachedPreview::Binary => "<Binary file>",
                CachedPreview::LargeFile => "<File too large to preview>",
                CachedPreview::NotFound => "<File not found>",
            },
        }
    }
}

impl<T: Item> FilePicker<T> {
    pub fn new(
        options: Vec<T>,
        editor_data: T::Data,
        callback_fn: impl Fn(&mut Context, &T, Action) + 'static,
        preview_fn: impl Fn(&Editor, &T) -> Option<FileLocation> + 'static,
    ) -> Self {
        let truncate_start = true;
        let mut picker = Picker::new(options, editor_data, callback_fn);
        picker.truncate_start = truncate_start;

        Self {
            picker,
            truncate_start,
            preview_cache: HashMap::new(),
            read_buffer: Vec::with_capacity(1024),
            file_fn: Box::new(preview_fn),
        }
    }

    pub fn truncate_start(mut self, truncate_start: bool) -> Self {
        self.truncate_start = truncate_start;
        self.picker.truncate_start = truncate_start;
        self
    }

    fn current_file(&self, editor: &Editor) -> Option<FileLocation> {
        self.picker
            .selection()
            .and_then(|current| (self.file_fn)(editor, current))
            .and_then(|(path_or_id, line)| path_or_id.get_canonicalized().ok().zip(Some(line)))
    }

    /// Get (cached) preview for a given path. If a document corresponding
    /// to the path is already open in the editor, it is used instead.
    fn get_preview<'picker, 'editor>(
        &'picker mut self,
        path_or_id: PathOrId,
        editor: &'editor Editor,
    ) -> Preview<'picker, 'editor> {
        match path_or_id {
            PathOrId::Path(path) => {
                let path = &path;
                if let Some(doc) = editor.document_by_path(path) {
                    return Preview::EditorDocument(doc);
                }

                if self.preview_cache.contains_key(path) {
                    return Preview::Cached(&self.preview_cache[path]);
                }

                let data = std::fs::File::open(path).and_then(|file| {
                    let metadata = file.metadata()?;
                    // Read up to 1kb to detect the content type
                    let n = file.take(1024).read_to_end(&mut self.read_buffer)?;
                    let content_type = content_inspector::inspect(&self.read_buffer[..n]);
                    self.read_buffer.clear();
                    Ok((metadata, content_type))
                });
                let preview = data
                    .map(
                        |(metadata, content_type)| match (metadata.len(), content_type) {
                            (_, content_inspector::ContentType::BINARY) => CachedPreview::Binary,
                            (size, _) if size > MAX_FILE_SIZE_FOR_PREVIEW => {
                                CachedPreview::LargeFile
                            }
                            _ => {
                                // TODO: enable syntax highlighting; blocked by async rendering
                                Document::open(path, None, None)
                                    .map(|doc| CachedPreview::Document(Box::new(doc)))
                                    .unwrap_or(CachedPreview::NotFound)
                            }
                        },
                    )
                    .unwrap_or(CachedPreview::NotFound);
                self.preview_cache.insert(path.to_owned(), preview);
                Preview::Cached(&self.preview_cache[path])
            }
            PathOrId::Id(id) => {
                let doc = editor.documents.get(&id).unwrap();
                Preview::EditorDocument(doc)
            }
        }
    }

    fn handle_idle_timeout(&mut self, cx: &mut Context) -> EventResult {
        // Try to find a document in the cache
        let doc = self
            .current_file(cx.editor)
            .and_then(|(path, _range)| match path {
                PathOrId::Id(doc_id) => Some(doc_mut!(cx.editor, &doc_id)),
                PathOrId::Path(path) => match self.preview_cache.get_mut(&path) {
                    Some(CachedPreview::Document(doc)) => Some(doc),
                    _ => None,
                },
            });

        // Then attempt to highlight it if it has no language set
        if let Some(doc) = doc {
            if doc.language_config().is_none() {
                let loader = cx.editor.syn_loader.clone();
                doc.detect_language(loader);
            }
        }

        EventResult::Consumed(None)
    }
}

impl<T: Item + 'static> Component for FilePicker<T> {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // +---------+ +---------+
        // |prompt   | |preview  |
        // +---------+ |         |
        // |picker   | |         |
        // |         | |         |
        // +---------+ +---------+

        let render_preview = self.picker.show_preview && area.width > MIN_AREA_WIDTH_FOR_PREVIEW;
        // -- Render the frame:
        // clear area
        let background = cx.editor.theme.get("ui.background");
        let text = cx.editor.theme.get("ui.text");
        surface.clear_with(area, background);

        let picker_width = if render_preview {
            area.width / 2
        } else {
            area.width
        };

        let picker_area = area.with_width(picker_width);
        self.picker.render(picker_area, surface, cx);

        if !render_preview {
            return;
        }

        let preview_area = area.clip_left(picker_width);

        // don't like this but the lifetime sucks
        let block = Block::default().borders(Borders::ALL);

        // calculate the inner area inside the box
        let inner = block.inner(preview_area);
        // 1 column gap on either side
        let margin = Margin::horizontal(1);
        let inner = inner.inner(&margin);
        block.render(preview_area, surface);

        if let Some((path, range)) = self.current_file(cx.editor) {
            let preview = self.get_preview(path, cx.editor);
            let doc = match preview.document() {
                Some(doc) => doc,
                None => {
                    let alt_text = preview.placeholder();
                    let x = inner.x + inner.width.saturating_sub(alt_text.len() as u16) / 2;
                    let y = inner.y + inner.height / 2;
                    surface.set_stringn(x, y, alt_text, inner.width as usize, text);
                    return;
                }
            };

            // align to middle
            let first_line = range
                .map(|(start, end)| {
                    let height = end.saturating_sub(start) + 1;
                    let middle = start + (height.saturating_sub(1) / 2);
                    middle.saturating_sub(inner.height as usize / 2).min(start)
                })
                .unwrap_or(0);

            let offset = Position::new(first_line, 0);

            let mut highlights =
                EditorView::doc_syntax_highlights(doc, offset, area.height, &cx.editor.theme);
            for spans in EditorView::doc_diagnostics_highlights(doc, &cx.editor.theme) {
                if spans.is_empty() {
                    continue;
                }
                highlights = Box::new(helix_core::syntax::merge(highlights, spans));
            }
            EditorView::render_text_highlights(
                doc,
                offset,
                inner,
                surface,
                &cx.editor.theme,
                highlights,
                &cx.editor.config(),
            );

            // highlight the line
            if let Some((start, end)) = range {
                let offset = start.saturating_sub(first_line) as u16;
                surface.set_style(
                    Rect::new(
                        inner.x,
                        inner.y + offset,
                        inner.width,
                        (end.saturating_sub(start) as u16 + 1)
                            .min(inner.height.saturating_sub(offset)),
                    ),
                    cx.editor
                        .theme
                        .try_get("ui.highlight")
                        .unwrap_or_else(|| cx.editor.theme.get("ui.selection")),
                );
            }
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &mut Context) -> EventResult {
        if let Event::IdleTimeout = event {
            return self.handle_idle_timeout(ctx);
        }
        // TODO: keybinds for scrolling preview
        self.picker.handle_event(event, ctx)
    }

    fn cursor(&self, area: Rect, ctx: &Editor) -> (Option<Position>, CursorKind) {
        self.picker.cursor(area, ctx)
    }

    fn required_size(&mut self, (width, height): (u16, u16)) -> Option<(u16, u16)> {
        let picker_width = if width > MIN_AREA_WIDTH_FOR_PREVIEW {
            width / 2
        } else {
            width
        };
        self.picker.required_size((picker_width, height))?;
        Some((width, height))
    }
}

#[derive(PartialEq, Eq, Debug)]
struct PickerMatch {
    index: usize,
    score: i64,
    len: usize,
}

impl PartialOrd for PickerMatch {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PickerMatch {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .cmp(&other.score)
            .reverse()
            .then_with(|| self.len.cmp(&other.len))
    }
}

pub struct Picker<T: Item> {
    options: Vec<T>,
    editor_data: T::Data,
    // filter: String,
    matcher: Box<Matcher>,
    matches: Vec<PickerMatch>,

    /// Current height of the completions box
    completion_height: u16,

    cursor: usize,
    // pattern: String,
    prompt: Prompt,
    previous_pattern: String,
    /// Whether to truncate the start (default true)
    pub truncate_start: bool,
    /// Whether to show the preview panel (default true)
    show_preview: bool,

    callback_fn: Box<dyn Fn(&mut Context, &T, Action)>,
}

impl<T: Item> Picker<T> {
    pub fn new(
        options: Vec<T>,
        editor_data: T::Data,
        callback_fn: impl Fn(&mut Context, &T, Action) + 'static,
    ) -> Self {
        let prompt = Prompt::new(
            "".into(),
            None,
            ui::completers::none,
            |_editor: &mut Context, _pattern: &str, _event: PromptEvent| {},
        );

        let mut picker = Self {
            options,
            editor_data,
            matcher: Box::new(Matcher::default()),
            matches: Vec::new(),
            cursor: 0,
            prompt,
            previous_pattern: String::new(),
            truncate_start: true,
            show_preview: true,
            callback_fn: Box::new(callback_fn),
            completion_height: 0,
        };

        // scoring on empty input:
        // TODO: just reuse score()
        picker
            .matches
            .extend(picker.options.iter().enumerate().map(|(index, option)| {
                let text = option.filter_text(&picker.editor_data);
                PickerMatch {
                    index,
                    score: 0,
                    len: text.chars().count(),
                }
            }));

        picker
    }

    pub fn score(&mut self) {
        let now = Instant::now();

        let pattern = self.prompt.line();

        if pattern == &self.previous_pattern {
            return;
        }

        if pattern.is_empty() {
            // Fast path for no pattern.
            self.matches.clear();
            self.matches
                .extend(self.options.iter().enumerate().map(|(index, option)| {
                    let text = option.filter_text(&self.editor_data);
                    PickerMatch {
                        index,
                        score: 0,
                        len: text.chars().count(),
                    }
                }));
        } else if pattern.starts_with(&self.previous_pattern) {
            let query = FuzzyQuery::new(pattern);
            // optimization: if the pattern is a more specific version of the previous one
            // then we can score the filtered set.
            self.matches.retain_mut(|pmatch| {
                let option = &self.options[pmatch.index];
                let text = option.sort_text(&self.editor_data);

                match query.fuzzy_match(&text, &self.matcher) {
                    Some(s) => {
                        // Update the score
                        pmatch.score = s;
                        true
                    }
                    None => false,
                }
            });

            self.matches.sort_unstable();
        } else {
            self.force_score();
        }

        log::debug!("picker score {:?}", Instant::now().duration_since(now));

        // reset cursor position
        self.cursor = 0;
        let pattern = self.prompt.line();
        self.previous_pattern.clone_from(pattern);
    }

    pub fn force_score(&mut self) {
        let pattern = self.prompt.line();

        let query = FuzzyQuery::new(pattern);
        self.matches.clear();
        self.matches.extend(
            self.options
                .iter()
                .enumerate()
                .filter_map(|(index, option)| {
                    let text = option.filter_text(&self.editor_data);

                    query
                        .fuzzy_match(&text, &self.matcher)
                        .map(|score| PickerMatch {
                            index,
                            score,
                            len: text.chars().count(),
                        })
                }),
        );
        self.matches.sort_unstable();
    }

    /// Move the cursor by a number of lines, either down (`Forward`) or up (`Backward`)
    pub fn move_by(&mut self, amount: usize, direction: Direction) {
        let len = self.matches.len();

        if len == 0 {
            // No results, can't move.
            return;
        }

        match direction {
            Direction::Forward => {
                self.cursor = self.cursor.saturating_add(amount) % len;
            }
            Direction::Backward => {
                self.cursor = self.cursor.saturating_add(len).saturating_sub(amount) % len;
            }
        }
    }

    /// Move the cursor down by exactly one page. After the last page comes the first page.
    pub fn page_up(&mut self) {
        self.move_by(self.completion_height as usize, Direction::Backward);
    }

    /// Move the cursor up by exactly one page. After the first page comes the last page.
    pub fn page_down(&mut self) {
        self.move_by(self.completion_height as usize, Direction::Forward);
    }

    /// Move the cursor to the first entry
    pub fn to_start(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor to the last entry
    pub fn to_end(&mut self) {
        self.cursor = self.matches.len().saturating_sub(1);
    }

    pub fn selection(&self) -> Option<&T> {
        self.matches
            .get(self.cursor)
            .map(|pmatch| &self.options[pmatch.index])
    }

    pub fn toggle_preview(&mut self) {
        self.show_preview = !self.show_preview;
    }

    fn prompt_handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        if let EventResult::Consumed(_) = self.prompt.handle_event(event, cx) {
            // TODO: recalculate only if pattern changed
            self.score();
        }
        EventResult::Consumed(None)
    }
}

// process:
// - read all the files into a list, maxed out at a large value
// - on input change:
//  - score all the names in relation to input

impl<T: Item + 'static> Component for Picker<T> {
    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        self.completion_height = viewport.1.saturating_sub(4);
        Some(viewport)
    }

    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        let key_event = match event {
            Event::Key(event) => *event,
            Event::Paste(..) => return self.prompt_handle_event(event, cx),
            Event::Resize(..) => return EventResult::Consumed(None),
            _ => return EventResult::Ignored(None),
        };

        let close_fn = EventResult::Consumed(Some(Box::new(|compositor: &mut Compositor, _cx| {
            // remove the layer
            compositor.last_picker = compositor.pop();
        })));

        // So that idle timeout retriggers
        cx.editor.reset_idle_timer();

        match key_event {
            shift!(Tab) | key!(Up) | ctrl!('p') => {
                self.move_by(1, Direction::Backward);
            }
            key!(Tab) | key!(Down) | ctrl!('n') => {
                self.move_by(1, Direction::Forward);
            }
            key!(PageDown) | ctrl!('d') => {
                self.page_down();
            }
            key!(PageUp) | ctrl!('u') => {
                self.page_up();
            }
            key!(Home) => {
                self.to_start();
            }
            key!(End) => {
                self.to_end();
            }
            key!(Esc) | ctrl!('c') => {
                return close_fn;
            }
            key!(Enter) => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(cx, option, Action::Replace);
                }
                return close_fn;
            }
            ctrl!('s') => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(cx, option, Action::HorizontalSplit);
                }
                return close_fn;
            }
            ctrl!('v') => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(cx, option, Action::VerticalSplit);
                }
                return close_fn;
            }
            ctrl!('t') => {
                self.toggle_preview();
            }
            _ => {
                self.prompt_handle_event(event, cx);
            }
        }

        EventResult::Consumed(None)
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let text_style = cx.editor.theme.get("ui.text");
        let selected = cx.editor.theme.get("ui.text.focus");
        let highlighted = cx.editor.theme.get("special").add_modifier(Modifier::BOLD);

        // -- Render the frame:
        // clear area
        let background = cx.editor.theme.get("ui.background");
        surface.clear_with(area, background);

        // don't like this but the lifetime sucks
        let block = Block::default().borders(Borders::ALL);

        // calculate the inner area inside the box
        let inner = block.inner(area);

        block.render(area, surface);

        // -- Render the input bar:

        let area = inner.clip_left(1).with_height(1);

        let count = format!("{}/{}", self.matches.len(), self.options.len());
        surface.set_stringn(
            (area.x + area.width).saturating_sub(count.len() as u16 + 1),
            area.y,
            &count,
            (count.len()).min(area.width as usize),
            text_style,
        );

        self.prompt.render(area, surface, cx);

        // -- Separator
        let sep_style = cx.editor.theme.get("ui.background.separator");
        let borders = BorderType::line_symbols(BorderType::Plain);
        for x in inner.left()..inner.right() {
            if let Some(cell) = surface.get_mut(x, inner.y + 1) {
                cell.set_symbol(borders.horizontal).set_style(sep_style);
            }
        }

        // -- Render the contents:
        // subtract area of prompt from top and current item marker " > " from left
        let inner = inner.clip_top(2).clip_left(3);

        let rows = inner.height;
        let offset = self.cursor - (self.cursor % std::cmp::max(1, rows as usize));

        let files = self
            .matches
            .iter()
            .skip(offset)
            .map(|pmatch| (pmatch.index, self.options.get(pmatch.index).unwrap()));

        for (i, (_index, option)) in files.take(rows as usize).enumerate() {
            let is_active = i == (self.cursor - offset);
            if is_active {
                surface.set_string(
                    inner.x.saturating_sub(3),
                    inner.y + i as u16,
                    " > ",
                    selected,
                );
                surface.set_style(
                    Rect::new(inner.x, inner.y + i as u16, inner.width, 1),
                    selected,
                );
            }

            let spans = option.label(&self.editor_data);
            let (_score, highlights) = FuzzyQuery::new(self.prompt.line())
                .fuzzy_indicies(&String::from(&spans), &self.matcher)
                .unwrap_or_default();

            spans.0.into_iter().fold(inner, |pos, span| {
                let new_x = surface
                    .set_string_truncated(
                        pos.x,
                        pos.y + i as u16,
                        &span.content,
                        pos.width as usize,
                        |idx| {
                            if highlights.contains(&idx) {
                                highlighted.patch(span.style)
                            } else if is_active {
                                selected.patch(span.style)
                            } else {
                                text_style.patch(span.style)
                            }
                        },
                        true,
                        self.truncate_start,
                    )
                    .0;
                pos.clip_left(new_x - pos.x)
            });
        }
    }

    fn cursor(&self, area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        let block = Block::default().borders(Borders::ALL);
        // calculate the inner area inside the box
        let inner = block.inner(area);

        // prompt area
        let area = inner.clip_left(1).with_height(1);

        self.prompt.cursor(area, editor)
    }
}

/// Returns a new list of options to replace the contents of the picker
/// when called with the current picker query,
pub type DynQueryCallback<T> =
    Box<dyn Fn(String, &mut Editor) -> BoxFuture<'static, anyhow::Result<Vec<T>>>>;

/// A picker that updates its contents via a callback whenever the
/// query string changes. Useful for live grep, workspace symbols, etc.
pub struct DynamicPicker<T: ui::menu::Item + Send> {
    file_picker: FilePicker<T>,
    query_callback: DynQueryCallback<T>,
    query: String,
}

impl<T: ui::menu::Item + Send> DynamicPicker<T> {
    pub const ID: &'static str = "dynamic-picker";

    pub fn new(file_picker: FilePicker<T>, query_callback: DynQueryCallback<T>) -> Self {
        Self {
            file_picker,
            query_callback,
            query: String::new(),
        }
    }
}

impl<T: Item + Send + 'static> Component for DynamicPicker<T> {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        self.file_picker.render(area, surface, cx);
    }

    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        let event_result = self.file_picker.handle_event(event, cx);
        let current_query = self.file_picker.picker.prompt.line();

        if !matches!(event, Event::IdleTimeout) || self.query == *current_query {
            return event_result;
        }

        self.query.clone_from(current_query);

        let new_options = (self.query_callback)(current_query.to_owned(), cx.editor);

        cx.jobs.callback(async move {
            let new_options = new_options.await?;
            let callback =
                crate::job::Callback::EditorCompositor(Box::new(move |editor, compositor| {
                    // Wrapping of pickers in overlay is done outside the picker code,
                    // so this is fragile and will break if wrapped in some other widget.
                    let picker = match compositor.find_id::<Overlay<DynamicPicker<T>>>(Self::ID) {
                        Some(overlay) => &mut overlay.content.file_picker.picker,
                        None => return,
                    };
                    picker.options = new_options;
                    picker.cursor = 0;
                    picker.force_score();
                    editor.reset_idle_timer();
                }));
            anyhow::Ok(callback)
        });
        EventResult::Consumed(None)
    }

    fn cursor(&self, area: Rect, ctx: &Editor) -> (Option<Position>, CursorKind) {
        self.file_picker.cursor(area, ctx)
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        self.file_picker.required_size(viewport)
    }

    fn id(&self) -> Option<&'static str> {
        Some(Self::ID)
    }
}
