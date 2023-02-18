use super::{util, BSCtx, BSCtxRef, BSNode, BWidget, Widget, WidgetPath, DEFAULT_PROPS};
use crate::{bscript::LocalEvent, view};
use futures::channel::oneshot;
use gdk::{self, prelude::*};
use glib::idle_add_local_once;
use gtk4::{self as gtk, prelude::*, Orientation};
use netidx::{chars::Chars, path::Path};
use netidx_bscript::vm;
use std::{cell::RefCell, cmp::max, rc::Rc};

pub(crate) fn dir_to_gtk(d: &view::Direction) -> gtk::Orientation {
    match d {
        view::Direction::Horizontal => Orientation::Horizontal,
        view::Direction::Vertical => Orientation::Vertical,
    }
}

pub(super) struct Paned {
    root: gtk::Paned,
    first_child: Option<Widget>,
    second_child: Option<Widget>,
}

impl Paned {
    pub(super) fn new(
        ctx: &BSCtx,
        spec: view::Paned,
        scope: Path,
        selected_path: gtk::Label,
    ) -> Self {
        let scope = scope.append("p");
        let root = gtk::Paned::new(dir_to_gtk(&spec.direction));
        root.set_no_show_all(true);
        root.set_wide_handle(spec.wide_handle);
        let first_child = spec.first_child.map(|child| {
            let w =
                Widget::new(ctx, (*child).clone(), scope.clone(), selected_path.clone());
            if let Some(w) = w.root() {
                root.pack1(w, true, true);
            }
            w
        });
        let second_child = spec.second_child.map(|child| {
            let w = Widget::new(ctx, (*child).clone(), scope, selected_path.clone());
            if let Some(w) = w.root() {
                root.pack2(w, true, true);
            }
            w
        });
        idle_add_local_once(clone!(@weak root => move || {
            root.set_position_set(true);
        }));
        Paned { root, first_child, second_child }
    }
}

impl BWidget for Paned {
    fn update(
        &mut self,
        ctx: BSCtxRef,
        waits: &mut Vec<oneshot::Receiver<()>>,
        event: &vm::Event<LocalEvent>,
    ) {
        if let Some(c) = &mut self.first_child {
            c.update(ctx, waits, event);
        }
        if let Some(c) = &mut self.second_child {
            c.update(ctx, waits, event);
        }
    }

    fn root(&self) -> Option<&gtk::Widget> {
        Some(self.root.upcast_ref())
    }

    fn set_highlight(&self, mut path: std::slice::Iter<WidgetPath>, h: bool) {
        match path.next() {
            Some(WidgetPath::Leaf) => util::set_highlight(&self.root, h),
            Some(WidgetPath::Box(i)) => {
                let c = if *i == 0 {
                    &self.first_child
                } else if *i == 1 {
                    &self.second_child
                } else {
                    &None
                };
                if let Some(c) = c {
                    c.set_highlight(path, h)
                }
            }
            _ => (),
        }
    }
}

pub(super) struct Frame {
    root: gtk::Frame,
    label: BSNode,
    child: Option<Widget>,
}

impl Frame {
    pub(super) fn new(
        ctx: &BSCtx,
        spec: view::Frame,
        scope: Path,
        selected_path: gtk::Label,
    ) -> Self {
        let label = BSNode::compile(&mut ctx.borrow_mut(), scope.clone(), spec.label);
        let label_val =
            label.current(&mut ctx.borrow_mut()).and_then(|v| v.get_as::<Chars>());
        let label_val = label_val.as_ref().map(|s| s.as_ref());
        let root = gtk::Frame::new(label_val);
        root.set_no_show_all(true);
        root.set_label_align(spec.label_align_horizontal);
        let child = spec.child.map(|child| {
            let w =
                Widget::new(ctx, (*child).clone(), scope.clone(), selected_path.clone());
            if let Some(w) = w.root() {
                root.add(w);
            }
            w
        });
        Frame { root, label, child }
    }
}

impl BWidget for Frame {
    fn update(
        &mut self,
        ctx: BSCtxRef,
        waits: &mut Vec<oneshot::Receiver<()>>,
        event: &vm::Event<LocalEvent>,
    ) {
        if let Some(new_lbl) = self.label.update(ctx, event) {
            self.root.set_label(new_lbl.get_as::<Chars>().as_ref().map(|c| c.as_ref()));
        }
        if let Some(c) = &mut self.child {
            c.update(ctx, waits, event);
        }
    }

    fn root(&self) -> Option<&gtk::Widget> {
        Some(self.root.upcast_ref())
    }

    fn set_highlight(&self, mut path: std::slice::Iter<WidgetPath>, h: bool) {
        match path.next() {
            Some(WidgetPath::Leaf) => util::set_highlight(&self.root, h),
            Some(WidgetPath::Box(i)) => {
                if *i == 0 {
                    if let Some(c) = &self.child {
                        c.set_highlight(path, h);
                    }
                }
            }
            _ => (),
        }
    }
}

pub(super) struct Notebook {
    root: gtk::Notebook,
    page: BSNode,
    on_switch_page: Rc<RefCell<BSNode>>,
    children: Vec<Widget>,
}

impl Notebook {
    pub(super) fn new(
        ctx: &BSCtx,
        spec: view::Notebook,
        scope: Path,
        selected_path: gtk::Label,
    ) -> Self {
        let scope = scope.append("n");
        let root = gtk::Notebook::new();
        root.set_no_show_all(true);
        let page = BSNode::compile(&mut *ctx.borrow_mut(), scope.clone(), spec.page);
        let on_switch_page = Rc::new(RefCell::new(BSNode::compile(
            &mut *ctx.borrow_mut(),
            scope.clone(),
            spec.on_switch_page,
        )));
        root.set_show_tabs(spec.tabs_visible);
        root.set_tab_pos(match spec.tabs_position {
            view::TabPosition::Left => gtk::PositionType::Left,
            view::TabPosition::Right => gtk::PositionType::Right,
            view::TabPosition::Top => gtk::PositionType::Top,
            view::TabPosition::Bottom => gtk::PositionType::Bottom,
        });
        root.set_enable_popup(spec.tabs_popup);
        let mut children = Vec::new();
        for s in spec.children.iter() {
            match &s.kind {
                view::WidgetKind::NotebookPage(view::NotebookPage {
                    label,
                    reorderable,
                    widget,
                }) => {
                    let w = Widget::new(
                        ctx,
                        (&**widget).clone(),
                        scope.clone(),
                        selected_path.clone(),
                    );
                    if let Some(r) = w.root() {
                        let lbl = gtk::Label::new(Some(label.as_str()));
                        root.append_page(r, Some(&lbl));
                        root.set_tab_reorderable(r, *reorderable);
                    }
                    children.push(w);
                }
                _ => {
                    let w =
                        Widget::new(ctx, s.clone(), scope.clone(), selected_path.clone());
                    if let Some(r) = w.root() {
                        root.append_page(r, None::<&gtk::Label>);
                    }
                    children.push(w);
                }
            }
        }
        root.set_current_page(
            page.current(&mut ctx.borrow_mut()).and_then(|v| v.get_as::<u32>()),
        );
        root.connect_switch_page(clone!(
        @strong ctx, @strong on_switch_page => move |_, _, page| {
            let ev = vm::Event::User(LocalEvent::Event(page.into()));
            on_switch_page.borrow_mut().update(&mut ctx.borrow_mut(), &ev);
        }));
        Notebook { root, page, on_switch_page, children }
    }
}

impl BWidget for Notebook {
    fn update(
        &mut self,
        ctx: BSCtxRef,
        waits: &mut Vec<oneshot::Receiver<()>>,
        event: &vm::Event<LocalEvent>,
    ) {
        if let Some(page) = self.page.update(ctx, event) {
            if let Some(page) = page.get_as::<u32>() {
                self.root.set_current_page(Some(page));
            }
        }
        self.on_switch_page.borrow_mut().update(ctx, event);
        for c in &mut self.children {
            c.update(ctx, waits, event);
        }
    }

    fn root(&self) -> Option<&gtk::Widget> {
        Some(self.root.upcast_ref())
    }

    fn set_highlight(&self, mut path: std::slice::Iter<WidgetPath>, h: bool) {
        match path.next() {
            Some(WidgetPath::Leaf) => util::set_highlight(&self.root, h),
            Some(WidgetPath::Box(i)) => {
                if let Some(c) = self.children.get(*i) {
                    c.set_highlight(path, h)
                }
            }
            _ => (),
        }
    }
}

pub(super) struct Box {
    root: gtk::Box,
    children: Vec<Widget>,
}

impl Box {
    pub(super) fn new(
        ctx: &BSCtx,
        spec: view::Box,
        scope: Path,
        selected_path: gtk::Label,
    ) -> Self {
        fn is_fill(a: view::Align) -> bool {
            match a {
                view::Align::Fill | view::Align::Baseline => true,
                view::Align::Start | view::Align::Center | view::Align::End => false,
            }
        }
        let scope = scope.append("b");
        let root = gtk::Box::new(dir_to_gtk(&spec.direction), 0);
        root.set_no_show_all(true);
        root.set_homogeneous(spec.homogeneous);
        root.set_spacing(spec.spacing as i32);
        let mut children = Vec::new();
        for s in spec.children.iter() {
            match &s.kind {
                view::WidgetKind::BoxChild(view::BoxChild { pack, padding, widget }) => {
                    let w = Widget::new(
                        ctx,
                        (&**widget).clone(),
                        scope.clone(),
                        selected_path.clone(),
                    );
                    if let Some(r) = w.root() {
                        let props = s.props.as_ref().unwrap_or(&DEFAULT_PROPS);
                        let (expand, fill) = match spec.direction {
                            view::Direction::Horizontal => {
                                (props.hexpand, is_fill(props.halign))
                            }
                            view::Direction::Vertical => {
                                (props.vexpand, is_fill(props.valign))
                            }
                        };
                        match pack {
                            view::Pack::Start => {
                                root.pack_start(r, expand, fill, *padding as u32)
                            }
                            view::Pack::End => {
                                root.pack_end(r, expand, fill, *padding as u32)
                            }
                        }
                    }
                    children.push(w);
                }
                _ => {
                    let w =
                        Widget::new(ctx, s.clone(), scope.clone(), selected_path.clone());
                    if let Some(r) = w.root() {
                        root.add(r);
                    }
                    children.push(w);
                }
            }
        }
        Box { root, children }
    }
}

impl BWidget for Box {
    fn update(
        &mut self,
        ctx: BSCtxRef,
        waits: &mut Vec<oneshot::Receiver<()>>,
        event: &vm::Event<LocalEvent>,
    ) {
        for c in &mut self.children {
            c.update(ctx, waits, event);
        }
    }

    fn root(&self) -> Option<&gtk::Widget> {
        Some(self.root.upcast_ref())
    }

    fn set_highlight(&self, mut path: std::slice::Iter<WidgetPath>, h: bool) {
        match path.next() {
            Some(WidgetPath::Leaf) => util::set_highlight(&self.root, h),
            Some(WidgetPath::Box(i)) => {
                if let Some(c) = self.children.get(*i) {
                    c.set_highlight(path, h)
                }
            }
            _ => (),
        }
    }
}

pub(super) struct Grid {
    root: gtk::Grid,
    children: Vec<Vec<Widget>>,
}

impl Grid {
    pub(super) fn new(
        ctx: &BSCtx,
        spec: view::Grid,
        scope: Path,
        selected_path: gtk::Label,
    ) -> Self {
        let scope = scope.append("g");
        let root = gtk::Grid::new();
        root.set_no_show_all(true);
        let attach_child = |spec: view::GridChild,
                            max_height: &mut i32,
                            i: &mut i32,
                            j: i32|
         -> Widget {
            let height = spec.height as i32;
            let width = spec.width as i32;
            let w = Widget::new(
                ctx,
                (&*spec.widget).clone(),
                scope.clone(),
                selected_path.clone(),
            );
            if let Some(r) = w.root() {
                root.attach(r, *i, j, width, height);
            }
            *i += width;
            *max_height = max(*max_height, height);
            w
        };
        let attach_normal = |spec: view::Widget, i: &mut i32, j: i32| -> Widget {
            let w = Widget::new(ctx, spec.clone(), scope.clone(), selected_path.clone());
            if let Some(r) = w.root() {
                root.attach(r, *i, j, 1, 1);
            }
            *i += 1;
            w
        };
        root.set_column_homogeneous(spec.homogeneous_columns);
        root.set_row_homogeneous(spec.homogeneous_rows);
        root.set_column_spacing(spec.column_spacing);
        root.set_row_spacing(spec.row_spacing);
        let mut i = 0i32;
        let mut j = 0i32;
        let children = spec
            .rows
            .into_iter()
            .map(|spec| {
                let mut max_height = 1;
                let row = match spec.kind {
                    view::WidgetKind::GridChild(c) => {
                        vec![attach_child(c, &mut max_height, &mut i, j)]
                    }
                    view::WidgetKind::GridRow(view::GridRow { columns }) => columns
                        .into_iter()
                        .map(|spec| match spec.kind {
                            view::WidgetKind::GridChild(c) => {
                                attach_child(c, &mut max_height, &mut i, j)
                            }
                            _ => attach_normal(spec, &mut i, j),
                        })
                        .collect(),
                    _ => vec![attach_normal(spec, &mut i, j)],
                };
                j += max_height;
                i = 0;
                row
            })
            .collect::<Vec<_>>();
        Grid { root, children }
    }
}

impl BWidget for Grid {
    fn update(
        &mut self,
        ctx: BSCtxRef,
        waits: &mut Vec<oneshot::Receiver<()>>,
        event: &vm::Event<LocalEvent>,
    ) {
        for row in &mut self.children {
            for child in row {
                child.update(ctx, waits, event);
            }
        }
    }

    fn root(&self) -> Option<&gtk::Widget> {
        Some(self.root.upcast_ref())
    }

    fn set_highlight(&self, mut path: std::slice::Iter<WidgetPath>, h: bool) {
        match path.next() {
            Some(WidgetPath::Leaf) => util::set_highlight(&self.root, h),
            Some(WidgetPath::GridItem(i, j)) => {
                if let Some(row) = self.children.get(*i) {
                    if let Some(c) = row.get(*j) {
                        c.set_highlight(path, h)
                    }
                }
            }
            Some(WidgetPath::GridRow(i)) => {
                if let Some(row) = self.children.get(*i) {
                    for c in row {
                        if let Some(r) = c.root() {
                            util::set_highlight(r, h)
                        }
                    }
                }
            }
            _ => (),
        }
    }
}
