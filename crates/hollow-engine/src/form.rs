//! HTML forms without scripts (MODE-02): the state of every control, what
//! clicks and keys do to it, and the entry list a submission sends.
//!
//! The state lives apart from the layout, so typing never relays out the
//! page: fields have a fixed size and their contents are drawn at paint time.
//! Nothing here touches the network; a submission is handed to the caller
//! (the renderer, which passes it to the broker) as plain data.

use std::collections::BTreeMap;

use url::Url;

use crate::dom::{Dom, NodeData, NodeId, collapse_whitespace};

/// Longest value one text control holds (also `maxlength`'s ceiling).
const MAX_VALUE: usize = 64 * 1024;
/// Controls tracked per document; later ones are inert.
const MAX_CONTROLS: usize = 10_000;
/// Options kept per `<select>`.
const MAX_OPTIONS: usize = 2_000;
/// Entries one submission may carry.
pub const MAX_ENTRIES: usize = 4_096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextKind {
    Text,
    Password,
    Multiline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ButtonKind {
    Submit,
    Reset,
    Plain,
}

#[derive(Clone, Debug)]
pub(crate) struct SelectOption {
    pub label: String,
    pub value: String,
    pub disabled: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum Control {
    Text { kind: TextKind, value: String, initial: String, caret: usize, max_len: usize, placeholder: String },
    Check { checked: bool, initial: bool, radio: bool, value: String },
    Select { options: Vec<SelectOption>, selected: Vec<bool>, initial: Vec<bool>, multiple: bool },
    Button { kind: ButtonKind, value: String },
}

#[derive(Clone, Debug)]
struct Entry {
    node: NodeId,
    name: String,
    form: Option<NodeId>,
    disabled: bool,
    readonly: bool,
    control: Control,
}

/// A key the user pressed while a page field had focus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormKey {
    Text(String),
    Backspace,
    Delete,
    Enter,
    Left,
    Right,
    Home,
    End,
    Up,
    Down,
    Tab,
    BackTab,
    Escape,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormMethod {
    Get,
    Post,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormEnctype {
    UrlEncoded,
    Multipart,
    Plain,
}

impl FormEnctype {
    pub fn mime(self) -> &'static str {
        match self {
            FormEnctype::UrlEncoded => "application/x-www-form-urlencoded",
            FormEnctype::Multipart => "multipart/form-data",
            FormEnctype::Plain => "text/plain",
        }
    }
}

/// A form the user submitted: where to, how, and the entry list in tree
/// order. Encoding is the caller's job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submission {
    pub action: Url,
    pub method: FormMethod,
    pub enctype: FormEnctype,
    pub entries: Vec<(String, String)>,
    /// `target="_blank"`: the result belongs in a new tab.
    pub new_tab: bool,
}

/// What an interaction asks of the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Interaction {
    /// Nothing visible changed.
    None,
    /// Control state changed: paint again.
    Repaint,
    Submit(Submission),
}

/// Something under the pointer that forms care about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FormTarget {
    Control(NodeId),
    /// Option `index` of the open `<select>`'s list.
    Option(NodeId, usize),
}

/// Every form control of a document and its current state.
#[derive(Clone, Debug, Default)]
pub(crate) struct Forms {
    entries: Vec<Entry>,
    index: BTreeMap<NodeId, usize>,
    /// `<label>` elements and the control each one activates.
    labels: BTreeMap<NodeId, NodeId>,
    pub focused: Option<NodeId>,
    /// The `<select>` whose option list is showing.
    pub open: Option<NodeId>,
    /// Option highlighted in the open list.
    pub highlight: usize,
}

fn text_of(dom: &Dom, node: NodeId) -> String {
    let mut out = String::new();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        for &c in dom.nodes[n].children.iter().rev() {
            match &dom.nodes[c].data {
                NodeData::Text(t) => {
                    if out.len() < MAX_VALUE {
                        out.push_str(t);
                    }
                }
                NodeData::Element(_) => stack.push(c),
                NodeData::Root => {}
            }
        }
    }
    out
}

fn ancestors(dom: &Dom, node: NodeId) -> impl Iterator<Item = NodeId> + '_ {
    std::iter::successors(dom.nodes[node].parent, move |&n| dom.nodes[n].parent)
}

impl Forms {
    pub fn new(dom: &Dom) -> Forms {
        let mut forms = Forms::default();
        let mut ids: BTreeMap<&str, NodeId> = BTreeMap::new();
        for (n, node) in dom.nodes.iter().enumerate() {
            if let NodeData::Element(e) = &node.data
                && let Some(id) = &e.id
            {
                ids.entry(id.as_str()).or_insert(n);
            }
        }
        let is_form = |n: NodeId| dom.element(n).is_some_and(|e| e.name == "form");
        let mut label_for = Vec::new();
        for n in 0..dom.nodes.len() {
            let Some(e) = dom.element(n) else { continue };
            if e.name == "label" {
                label_for.push((n, e.attr("for").map(str::to_owned)));
                continue;
            }
            let control = match e.name.as_str() {
                "input" => {
                    let ty = e.attr("type").unwrap_or("text").trim().to_ascii_lowercase();
                    let value = e.attr("value").unwrap_or("").to_owned();
                    match ty.as_str() {
                        "checkbox" | "radio" => {
                            let checked = e.attr("checked").is_some();
                            let value = e.attr("value").unwrap_or("on").to_owned();
                            Control::Check { checked, initial: checked, radio: ty == "radio", value }
                        }
                        "submit" | "image" => Control::Button { kind: ButtonKind::Submit, value },
                        "reset" => Control::Button { kind: ButtonKind::Reset, value },
                        "button" => Control::Button { kind: ButtonKind::Plain, value },
                        // Files are never uploaded from Fortress; color and range
                        // keep their initial value.
                        "file" => continue,
                        _ => {
                            let kind = if ty == "password" { TextKind::Password } else { TextKind::Text };
                            let value = if ty == "hidden" { value } else { value.replace(['\r', '\n'], "") };
                            text_control(kind, value, e.attr("maxlength"), e.attr("placeholder"))
                        }
                    }
                }
                "textarea" => {
                    let mut value = text_of(dom, n).replace("\r\n", "\n");
                    // The parser drops a newline right after <textarea>.
                    if value.starts_with('\n') {
                        value.remove(0);
                    }
                    text_control(TextKind::Multiline, value, e.attr("maxlength"), e.attr("placeholder"))
                }
                "select" => select_control(dom, n, e.attr("multiple").is_some()),
                "button" => {
                    let kind = match e.attr("type").map(|t| t.trim().to_ascii_lowercase()).as_deref() {
                        Some("reset") => ButtonKind::Reset,
                        Some("button") => ButtonKind::Plain,
                        _ => ButtonKind::Submit,
                    };
                    Control::Button { kind, value: e.attr("value").unwrap_or("").to_owned() }
                }
                _ => continue,
            };
            if forms.entries.len() >= MAX_CONTROLS {
                break;
            }
            let form = match e.attr("form") {
                Some(id) => ids.get(id.trim()).copied().filter(|&f| is_form(f)),
                None => ancestors(dom, n).find(|&a| is_form(a)),
            };
            // A disabled <fieldset> disables what's inside, except its first <legend>.
            let disabled = e.attr("disabled").is_some()
                || ancestors(dom, n)
                    .any(|a| dom.element(a).is_some_and(|f| f.name == "fieldset" && f.attr("disabled").is_some()));
            forms.index.insert(n, forms.entries.len());
            forms.entries.push(Entry {
                node: n,
                name: e.attr("name").unwrap_or("").to_owned(),
                form,
                disabled,
                readonly: e.attr("readonly").is_some(),
                control,
            });
        }
        for (label, target) in label_for {
            let control = match target {
                Some(id) => ids.get(id.trim()).copied().filter(|c| forms.index.contains_key(c)),
                None => forms.entries.iter().map(|e| e.node).find(|&c| ancestors(dom, c).any(|a| a == label)),
            };
            if let Some(c) = control {
                forms.labels.insert(label, c);
            }
        }
        forms
    }

    fn entry(&self, node: NodeId) -> Option<&Entry> {
        self.index.get(&node).map(|&i| &self.entries[i])
    }

    fn entry_mut(&mut self, node: NodeId) -> Option<&mut Entry> {
        self.index.get(&node).map(|&i| &mut self.entries[i])
    }

    pub fn control(&self, node: NodeId) -> Option<&Control> {
        self.entry(node).map(|e| &e.control)
    }

    /// A form control or a `<label>` for one.
    pub fn is_control(&self, node: NodeId) -> bool {
        self.index.contains_key(&node) || self.labels.contains_key(&node)
    }

    pub fn is_disabled(&self, node: NodeId) -> bool {
        self.entry(node).is_some_and(|e| e.disabled)
    }

    /// Whether keys should go to a field rather than scroll the page.
    pub fn editing(&self) -> bool {
        self.open.is_some()
            || self.focused.and_then(|f| self.control(f)).is_some_and(|c| matches!(c, Control::Text { .. }))
    }

    pub fn click(&mut self, dom: &Dom, doc_url: &Url, target: Option<FormTarget>) -> Interaction {
        let was = (self.focused, self.open);
        let target = match target {
            Some(FormTarget::Control(n)) => Some(FormTarget::Control(self.labels.get(&n).copied().unwrap_or(n))),
            t => t,
        };
        match target {
            Some(FormTarget::Option(select, i)) if !self.is_disabled(select) => {
                self.open = None;
                self.focused = Some(select);
                match self.entry_mut(select).map(|e| &mut e.control) {
                    // A list box with `multiple` toggles the clicked option.
                    Some(Control::Select { options, selected, multiple: true, .. }) => {
                        if options.get(i).is_some_and(|o| !o.disabled) {
                            selected[i] = !selected[i];
                        }
                    }
                    _ => self.choose(select, i),
                }
                return Interaction::Repaint;
            }
            Some(FormTarget::Control(n)) if !self.is_disabled(n) => {
                self.open = None;
                self.focused = Some(n);
                let Some(entry) = self.entry_mut(n) else { return Interaction::None };
                let readonly = entry.readonly;
                match &mut entry.control {
                    Control::Text { value, caret, .. } => {
                        *caret = value.chars().count();
                        return Interaction::Repaint;
                    }
                    Control::Check { checked, radio: false, .. } => *checked = !*checked,
                    Control::Check { radio: true, .. } => self.check_radio(n),
                    Control::Select { multiple: false, selected, .. } => {
                        if !readonly {
                            self.highlight = selected.iter().position(|s| *s).unwrap_or(0);
                            self.open = Some(n);
                        }
                    }
                    Control::Select { .. } => {}
                    Control::Button { kind: ButtonKind::Submit, .. } => {
                        return self.submit(dom, doc_url, n).map_or(Interaction::Repaint, Interaction::Submit);
                    }
                    Control::Button { kind: ButtonKind::Reset, .. } => {
                        let form = self.entry(n).and_then(|e| e.form);
                        self.reset(form);
                    }
                    Control::Button { kind: ButtonKind::Plain, .. } => {}
                }
                return Interaction::Repaint;
            }
            _ => {
                self.focused = None;
                self.open = None;
            }
        }
        if (self.focused, self.open) == was { Interaction::None } else { Interaction::Repaint }
    }

    fn choose(&mut self, select: NodeId, i: usize) {
        if let Some(Control::Select { options, selected, .. }) = self.entry_mut(select).map(|e| &mut e.control)
            && options.get(i).is_some_and(|o| !o.disabled)
        {
            selected.iter_mut().for_each(|s| *s = false);
            selected[i] = true;
        }
    }

    fn check_radio(&mut self, n: NodeId) {
        let Some(e) = self.entry(n) else { return };
        let (name, form) = (e.name.clone(), e.form);
        for other in &mut self.entries {
            if let Control::Check { checked, radio: true, .. } = &mut other.control {
                if other.node == n {
                    *checked = true;
                } else if !name.is_empty() && other.name == name && other.form == form {
                    *checked = false;
                }
            }
        }
    }

    fn reset(&mut self, form: Option<NodeId>) {
        for e in self.entries.iter_mut().filter(|e| e.form == form) {
            match &mut e.control {
                Control::Text { value, initial, caret, .. } => {
                    *value = initial.clone();
                    *caret = value.chars().count();
                }
                Control::Check { checked, initial, .. } => *checked = *initial,
                Control::Select { selected, initial, .. } => selected.clone_from(initial),
                Control::Button { .. } => {}
            }
        }
    }

    pub fn key(&mut self, dom: &Dom, doc_url: &Url, key: &FormKey) -> Interaction {
        if let Some(select) = self.open {
            let count = match self.control(select) {
                Some(Control::Select { options, .. }) => options.len(),
                _ => 0,
            };
            match key {
                FormKey::Up => self.highlight = self.highlight.saturating_sub(1),
                FormKey::Down => self.highlight = (self.highlight + 1).min(count.saturating_sub(1)),
                FormKey::Enter => {
                    self.open = None;
                    self.choose(select, self.highlight);
                }
                FormKey::Escape | FormKey::Tab | FormKey::BackTab => self.open = None,
                _ => return Interaction::None,
            }
            return Interaction::Repaint;
        }
        let Some(focused) = self.focused else { return Interaction::None };
        match key {
            FormKey::Tab | FormKey::BackTab => {
                self.move_focus(*key == FormKey::Tab);
                return Interaction::Repaint;
            }
            FormKey::Escape => {
                self.focused = None;
                return Interaction::Repaint;
            }
            _ => {}
        }
        let Some(entry) = self.entry_mut(focused) else { return Interaction::None };
        let readonly = entry.readonly || entry.disabled;
        match &mut entry.control {
            Control::Text { kind, value, caret, max_len, .. } => {
                let multiline = *kind == TextKind::Multiline;
                if *key == FormKey::Enter && !multiline {
                    // Implicit submission, as when a browser user presses Enter.
                    return self.submit(dom, doc_url, focused).map_or(Interaction::None, Interaction::Submit);
                }
                edit_text(value, caret, *max_len, multiline, readonly, key)
            }
            Control::Check { .. } | Control::Button { .. }
                if matches!(key, FormKey::Enter) || *key == FormKey::Text(" ".into()) =>
            {
                self.click(dom, doc_url, Some(FormTarget::Control(focused)))
            }
            Control::Select { selected, options, multiple: false, .. }
                if matches!(key, FormKey::Up | FormKey::Down) =>
            {
                let cur = selected.iter().position(|s| *s).unwrap_or(0);
                let next = if *key == FormKey::Up {
                    cur.saturating_sub(1)
                } else {
                    (cur + 1).min(options.len().saturating_sub(1))
                };
                self.choose(focused, next);
                Interaction::Repaint
            }
            _ => Interaction::None,
        }
    }

    fn move_focus(&mut self, forward: bool) {
        let focusable: Vec<NodeId> = self.entries.iter().filter(|e| !e.disabled).map(|e| e.node).collect();
        if focusable.is_empty() {
            self.focused = None;
            return;
        }
        let pos = self.focused.and_then(|f| focusable.iter().position(|&n| n == f));
        let next = match (pos, forward) {
            (Some(p), true) => (p + 1) % focusable.len(),
            (Some(p), false) => (p + focusable.len() - 1) % focusable.len(),
            (None, true) => 0,
            (None, false) => focusable.len() - 1,
        };
        self.focused = Some(focusable[next]);
        if let Some(Control::Text { value, caret, .. }) = self.entry_mut(focusable[next]).map(|e| &mut e.control) {
            *caret = value.chars().count();
        }
    }

    /// Builds the submission of the form that `from` belongs to, with
    /// `from` as the submitter if it's a submit button (HTML's "constructing
    /// the entry list"). `None` if `from` isn't in a form.
    fn submit(&self, dom: &Dom, doc_url: &Url, from: NodeId) -> Option<Submission> {
        let form = self.entry(from)?.form?;
        let form_el = dom.element(form)?;
        let submitter = self
            .entry(from)
            .filter(|e| matches!(e.control, Control::Button { kind: ButtonKind::Submit, .. }))
            .map(|e| e.node)
            .or_else(|| {
                // Implicit submission uses the form's first submit button.
                self.entries
                    .iter()
                    .find(|e| {
                        e.form == Some(form) && matches!(e.control, Control::Button { kind: ButtonKind::Submit, .. })
                    })
                    .map(|e| e.node)
            });
        if submitter.is_some_and(|s| self.is_disabled(s)) {
            return None;
        }
        let sub_el = submitter.and_then(|s| dom.element(s));
        let attr = |name: &str| {
            sub_el
                .and_then(|e| e.attr(&format!("form{name}")))
                .or_else(|| form_el.attr(name))
                .map(|v| v.trim().to_ascii_lowercase())
        };
        let action = submitter
            .and_then(|s| dom.nodes[s].url.clone())
            .or_else(|| dom.nodes[form].url.clone())
            .unwrap_or_else(|| doc_url.clone());
        let method = match attr("method").as_deref() {
            Some("post") => FormMethod::Post,
            _ => FormMethod::Get,
        };
        let enctype = match attr("enctype").as_deref() {
            Some("multipart/form-data") => FormEnctype::Multipart,
            Some("text/plain") => FormEnctype::Plain,
            _ => FormEnctype::UrlEncoded,
        };
        let new_tab = attr("target").as_deref() == Some("_blank");

        let mut entries = Vec::new();
        for e in self.entries.iter().filter(|e| e.form == Some(form) && !e.disabled) {
            if e.name.is_empty() {
                continue;
            }
            match &e.control {
                Control::Button { value, .. } if Some(e.node) == submitter => {
                    entries.push((e.name.clone(), value.clone()))
                }
                Control::Button { .. } => {}
                Control::Check { checked: true, value, .. } => entries.push((e.name.clone(), value.clone())),
                Control::Check { .. } => {}
                Control::Select { options, selected, .. } => {
                    for (o, _) in options.iter().zip(selected).filter(|(o, s)| **s && !o.disabled) {
                        entries.push((e.name.clone(), o.value.clone()));
                    }
                }
                Control::Text { kind, value, .. } => {
                    let value = if e.name.eq_ignore_ascii_case("_charset_") && value.is_empty() {
                        "UTF-8".to_owned()
                    } else if *kind == TextKind::Multiline {
                        value.replace('\n', "\r\n")
                    } else {
                        value.clone()
                    };
                    entries.push((e.name.clone(), value));
                }
            }
            if entries.len() >= MAX_ENTRIES {
                break;
            }
        }
        Some(Submission { action, method, enctype, entries, new_tab })
    }
}

fn text_control(kind: TextKind, value: String, max_len: Option<&str>, placeholder: Option<&str>) -> Control {
    let max_len = max_len.and_then(|m| m.trim().parse::<usize>().ok()).unwrap_or(MAX_VALUE).min(MAX_VALUE);
    let value: String = value.chars().take(MAX_VALUE).collect();
    let caret = value.chars().count();
    let placeholder = collapse_whitespace(placeholder.unwrap_or(""));
    Control::Text { kind, initial: value.clone(), value, caret, max_len, placeholder }
}

fn select_control(dom: &Dom, select: NodeId, multiple: bool) -> Control {
    let mut options = Vec::new();
    let mut selected = Vec::new();
    // Nodes are in document order, so a subtree is the run of nodes after
    // its root whose ancestors include it.
    let inside = |n: NodeId| ancestors(dom, n).any(|a| a == select);
    for n in (select + 1..dom.nodes.len()).take_while(|&n| inside(n)) {
        let Some(e) = dom.element(n).filter(|e| e.name == "option") else { continue };
        if options.len() >= MAX_OPTIONS {
            break;
        }
        let group_disabled = ancestors(dom, n)
            .take_while(|&a| a != select)
            .any(|a| dom.element(a).is_some_and(|g| g.attr("disabled").is_some()));
        let label = collapse_whitespace(&e.attr("label").map_or_else(|| text_of(dom, n), str::to_owned));
        let value = e.attr("value").map_or_else(|| collapse_whitespace(&text_of(dom, n)), str::to_owned);
        options.push(SelectOption { label, value, disabled: group_disabled || e.attr("disabled").is_some() });
        selected.push(e.attr("selected").is_some());
    }
    if !multiple {
        // At most one selected: the last `selected` wins, else the first enabled option.
        let last = selected.iter().rposition(|s| *s);
        selected.iter_mut().for_each(|s| *s = false);
        if let Some(i) = last.or_else(|| options.iter().position(|o| !o.disabled)) {
            selected[i] = true;
        }
    }
    let initial = selected.clone();
    Control::Select { options, selected, initial, multiple }
}

/// Applies an editing key to a text value with a caret (in chars).
fn edit_text(
    value: &mut String,
    caret: &mut usize,
    max_len: usize,
    multiline: bool,
    readonly: bool,
    key: &FormKey,
) -> Interaction {
    let len = value.chars().count();
    *caret = (*caret).min(len);
    let byte = |s: &String, c: usize| s.char_indices().nth(c).map_or(s.len(), |(b, _)| b);
    match key {
        FormKey::Left => *caret = caret.saturating_sub(1),
        FormKey::Right => *caret = (*caret + 1).min(len),
        FormKey::Home => *caret = 0,
        FormKey::End => *caret = len,
        _ if readonly => return Interaction::None,
        FormKey::Backspace if *caret > 0 => {
            let b = byte(value, *caret - 1);
            value.remove(b);
            *caret -= 1;
        }
        FormKey::Delete if *caret < len => {
            let b = byte(value, *caret);
            value.remove(b);
        }
        FormKey::Enter if multiline => {
            if len < max_len {
                let b = byte(value, *caret);
                value.insert(b, '\n');
                *caret += 1;
            }
        }
        FormKey::Text(t) => {
            let room = max_len.saturating_sub(len);
            let typed: String = t.chars().filter(|c| !c.is_control() || (multiline && *c == '\n')).take(room).collect();
            if typed.is_empty() {
                return Interaction::None;
            }
            let b = byte(value, *caret);
            value.insert_str(b, &typed);
            *caret += typed.chars().count();
        }
        _ => return Interaction::None,
    }
    Interaction::Repaint
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forms(html: &str) -> (Dom, Forms, Url) {
        let url = Url::parse("https://example.org/dir/page?x=1").unwrap();
        let dom = crate::dom::parse(html, &url).dom;
        let forms = Forms::new(&dom);
        (dom, forms, url)
    }

    fn node(dom: &Dom, name: &str, nth: usize) -> NodeId {
        (0..dom.nodes.len()).filter(|&n| dom.element(n).is_some_and(|e| e.name == name)).nth(nth).unwrap()
    }

    fn submitted(i: Interaction) -> Submission {
        match i {
            Interaction::Submit(s) => s,
            other => panic!("expected a submission, got {other:?}"),
        }
    }

    #[test]
    fn typing_and_enter_submit_a_search_form() {
        let (dom, mut f, url) = forms(
            "<form action='/search'><input type=hidden name=t value=h_><input name=q value='old'>\
             <input type=submit name=go value=Go></form>",
        );
        let q = node(&dom, "input", 1);
        assert_eq!(f.click(&dom, &url, Some(FormTarget::Control(q))), Interaction::Repaint);
        assert!(f.editing());
        for k in [FormKey::Backspace, FormKey::Backspace, FormKey::Backspace, FormKey::Text("tor é".into())] {
            f.key(&dom, &url, &k);
        }
        let s = submitted(f.key(&dom, &url, &FormKey::Enter));
        assert_eq!(s.action.as_str(), "https://example.org/search");
        assert_eq!(s.method, FormMethod::Get);
        // Implicit submission sends the default button too.
        let pairs: Vec<(&str, &str)> = s.entries.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        assert_eq!(pairs, [("t", "h_"), ("q", "tor é"), ("go", "Go")]);
    }

    #[test]
    fn checkboxes_radios_selects_and_the_submitter() {
        let (dom, mut f, url) = forms(
            "<form method=POST enctype=multipart/form-data target=_blank>\
             <input type=checkbox name=a checked><input type=checkbox name=b value=yes>\
             <input type=radio name=r value=1 checked><input type=radio name=r value=2>\
             <select name=s><option>one<option value=2 selected>two<option disabled>three</select>\
             <textarea name=t>\nline1\nline2</textarea><input name=off disabled value=x>\
             <button name=b1 value=v1>A</button><button name=b2 value=v2 formaction=/other>B</button></form>",
        );
        let input = |i| FormTarget::Control(node(&dom, "input", i));
        f.click(&dom, &url, Some(input(0)));
        f.click(&dom, &url, Some(input(1)));
        f.click(&dom, &url, Some(input(3)));
        let select = node(&dom, "select", 0);
        f.click(&dom, &url, Some(FormTarget::Control(select)));
        assert_eq!(f.open, Some(select));
        f.click(&dom, &url, Some(FormTarget::Option(select, 0)));
        let s = submitted(f.click(&dom, &url, Some(FormTarget::Control(node(&dom, "button", 1)))));
        assert_eq!(s.action.as_str(), "https://example.org/other");
        assert_eq!((s.method, s.enctype, s.new_tab), (FormMethod::Post, FormEnctype::Multipart, true));
        let pairs: Vec<(&str, &str)> = s.entries.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        assert_eq!(pairs, [("b", "yes"), ("r", "2"), ("s", "one"), ("t", "line1\r\nline2"), ("b2", "v2")]);
    }

    #[test]
    fn labels_reset_and_empty_action() {
        let (dom, mut f, url) = forms(
            "<form><label>Agree <input type=checkbox name=c></label><label for=n>Name</label>\
             <input id=n name=n value=init><input type=reset></form>",
        );
        let label = node(&dom, "label", 0);
        f.click(&dom, &url, Some(FormTarget::Control(label)));
        f.click(&dom, &url, Some(FormTarget::Control(node(&dom, "label", 1))));
        assert_eq!(f.focused, Some(node(&dom, "input", 1)));
        f.key(&dom, &url, &FormKey::Text("!".into()));
        let s = submitted(f.key(&dom, &url, &FormKey::Enter));
        // No action: the page itself.
        assert_eq!(s.action, url);
        let pairs: Vec<(&str, &str)> = s.entries.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        assert_eq!(pairs, [("c", "on"), ("n", "init!")]);
        // Reset puts everything back; the checkbox is unchecked again.
        f.click(&dom, &url, Some(FormTarget::Control(node(&dom, "input", 2))));
        f.click(&dom, &url, Some(FormTarget::Control(node(&dom, "label", 1))));
        let s = submitted(f.key(&dom, &url, &FormKey::Enter));
        assert_eq!(s.entries, [("n".to_owned(), "init".to_owned())]);
    }

    #[test]
    fn caret_editing_respects_maxlength_and_readonly() {
        let mut v = "abc".to_owned();
        let mut caret = 3;
        edit_text(&mut v, &mut caret, 5, false, false, &FormKey::Left);
        edit_text(&mut v, &mut caret, 5, false, false, &FormKey::Text("XYZ".into()));
        assert_eq!((v.as_str(), caret), ("abXYc", 4));
        edit_text(&mut v, &mut caret, 5, false, false, &FormKey::Home);
        edit_text(&mut v, &mut caret, 5, false, false, &FormKey::Delete);
        assert_eq!(v, "bXYc");
        assert_eq!(edit_text(&mut v, &mut caret, 5, false, true, &FormKey::Backspace), Interaction::None);
    }
}
