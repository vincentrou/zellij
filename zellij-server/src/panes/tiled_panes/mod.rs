mod pane_resizer;
mod stacked_panes;
mod tiled_pane_grid;

use crate::resize_pty;
use tiled_pane_grid::{split, TiledPaneGrid, RESIZE_PERCENT};

use crate::{
    os_input_output::ServerOsApi,
    output::Output,
    panes::{ActivePanes, PaneId},
    plugins::PluginInstruction,
    tab::{pane_info_for_pane, Pane, MIN_TERMINAL_HEIGHT, MIN_TERMINAL_WIDTH},
    thread_bus::ThreadSenders,
    ui::boundaries::Boundaries,
    ui::pane_contents_and_ui::PaneContentsAndUi,
    ClientId,
};
use stacked_panes::StackedPanes;
use zellij_utils::{
    data::{Direction, ModeInfo, PaneInfo, Resize, ResizeStrategy, Style, Styling},
    errors::prelude::*,
    input::{
        command::RunCommand,
        layout::{Run, RunPluginOrAlias, SplitDirection},
    },
    pane_size::{Offset, PaneGeom, Size, SizeInPixels, Viewport},
};

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet},
    rc::Rc,
    time::Instant,
};

fn pane_content_offset(position_and_size: &PaneGeom, viewport: &Viewport) -> (usize, usize) {
    // (columns_offset, rows_offset)
    // if the pane is not on the bottom or right edge on the screen, we need to reserve one space
    // from its content to leave room for the boundary between it and the next pane (if it doesn't
    // draw its own frame)
    let columns_offset = if position_and_size.x + position_and_size.cols.as_usize() < viewport.cols
    {
        1
    } else {
        0
    };
    let rows_offset = if position_and_size.y + position_and_size.rows.as_usize() < viewport.rows {
        1
    } else {
        0
    };
    (columns_offset, rows_offset)
}

pub struct TiledPanes {
    pub panes: BTreeMap<PaneId, Box<dyn Pane>>,
    display_area: Rc<RefCell<Size>>,
    viewport: Rc<RefCell<Viewport>>,
    connected_clients: Rc<RefCell<HashSet<ClientId>>>,
    connected_clients_in_app: Rc<RefCell<HashMap<ClientId, bool>>>, // bool -> is_web_client
    mode_info: Rc<RefCell<HashMap<ClientId, ModeInfo>>>,
    character_cell_size: Rc<RefCell<Option<SizeInPixels>>>,
    stacked_resize: Rc<RefCell<bool>>,
    default_mode_info: ModeInfo,
    style: Style,
    session_is_mirrored: bool,
    active_panes: ActivePanes,
    draw_pane_frames: bool,
    panes_to_hide: HashSet<PaneId>,
    fullscreen_is_active: Option<PaneId>,
    senders: ThreadSenders,
    window_title: Option<String>,
    client_id_to_boundaries: HashMap<ClientId, Boundaries>,
    tombstones_before_increase: Option<(PaneId, Vec<HashMap<PaneId, PaneGeom>>)>,
    tombstones_before_decrease: Option<(PaneId, Vec<HashMap<PaneId, PaneGeom>>)>,
}

impl TiledPanes {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        display_area: Rc<RefCell<Size>>,
        viewport: Rc<RefCell<Viewport>>,
        connected_clients: Rc<RefCell<HashSet<ClientId>>>,
        connected_clients_in_app: Rc<RefCell<HashMap<ClientId, bool>>>, // bool -> is_web_client
        mode_info: Rc<RefCell<HashMap<ClientId, ModeInfo>>>,
        character_cell_size: Rc<RefCell<Option<SizeInPixels>>>,
        stacked_resize: Rc<RefCell<bool>>,
        session_is_mirrored: bool,
        draw_pane_frames: bool,
        default_mode_info: ModeInfo,
        style: Style,
        os_api: Box<dyn ServerOsApi>,
        senders: ThreadSenders,
    ) -> Self {
        TiledPanes {
            panes: BTreeMap::new(),
            display_area,
            viewport,
            connected_clients,
            connected_clients_in_app,
            mode_info,
            character_cell_size,
            stacked_resize,
            default_mode_info,
            style,
            session_is_mirrored,
            active_panes: ActivePanes::new(&os_api),
            draw_pane_frames,
            panes_to_hide: HashSet::new(),
            fullscreen_is_active: None,
            senders,
            window_title: None,
            client_id_to_boundaries: HashMap::new(),
            tombstones_before_increase: None,
            tombstones_before_decrease: None,
        }
    }
    pub fn add_pane_with_existing_geom(&mut self, pane_id: PaneId, mut pane: Box<dyn Pane>) {
        if self.draw_pane_frames {
            pane.set_content_offset(Offset::frame(1));
        }
        self.panes.insert(pane_id, pane);
    }
    pub fn replace_active_pane(
        &mut self,
        pane: Box<dyn Pane>,
        client_id: ClientId,
    ) -> Option<Box<dyn Pane>> {
        let pane_id = pane.pid();
        // remove the currently active pane
        let previously_active_pane = self
            .active_panes
            .get(&client_id)
            .copied()
            .and_then(|active_pane_id| self.replace_pane(active_pane_id, pane));

        // move clients from the previously active pane to the new pane we just inserted
        if let Some(previously_active_pane) = previously_active_pane.as_ref() {
            let previously_active_pane_id = previously_active_pane.pid();
            self.move_clients_between_panes(previously_active_pane_id, pane_id);
        }
        previously_active_pane
    }
    pub fn replace_pane(
        &mut self,
        pane_id: PaneId,
        mut with_pane: Box<dyn Pane>,
    ) -> Option<Box<dyn Pane>> {
        let with_pane_id = with_pane.pid();
        if self.draw_pane_frames && !with_pane.borderless() {
            with_pane.set_content_offset(Offset::frame(1));
        }
        let removed_pane = self.panes.remove(&pane_id).map(|removed_pane| {
            let with_pane_id = with_pane.pid();
            let removed_pane_geom = removed_pane.position_and_size();
            let removed_pane_geom_override = removed_pane.geom_override();
            with_pane.set_geom(removed_pane_geom);
            match removed_pane_geom_override {
                Some(geom_override) => with_pane.set_geom_override(geom_override),
                None => with_pane.reset_size_and_position_override(),
            };
            self.panes.insert(with_pane_id, with_pane);
            removed_pane
        });

        // move clients from the previously active pane to the new pane we just inserted
        self.move_clients_between_panes(pane_id, with_pane_id);
        self.reapply_pane_frames();
        removed_pane
    }
    pub fn insert_pane(
        &mut self,
        pane_id: PaneId,
        pane: Box<dyn Pane>,
        client_id: Option<ClientId>,
    ) {
        let should_relayout = true;
        self.add_pane(pane_id, pane, should_relayout, client_id);
    }
    pub fn insert_pane_without_relayout(
        &mut self,
        pane_id: PaneId,
        pane: Box<dyn Pane>,
        client_id: Option<ClientId>,
    ) {
        let should_relayout = false;
        self.add_pane(pane_id, pane, should_relayout, client_id);
    }
    pub fn has_room_for_new_pane(&mut self) -> bool {
        let cursor_height_width_ratio = self.cursor_height_width_ratio();
        let pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        let has_room_for_new_pane = pane_grid
            .find_room_for_new_pane(cursor_height_width_ratio)
            .is_some();
        has_room_for_new_pane || pane_grid.has_room_for_new_stacked_pane() || self.panes.is_empty()
    }
    pub fn room_left_in_stack_of_pane_id(&mut self, pane_id: &PaneId) -> Option<usize> {
        let mut pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        pane_grid.room_left_in_stack_of_pane_id(pane_id)
    }

    pub fn assign_geom_for_pane_with_run(&mut self, run: Option<Run>) {
        // here we're removing the first pane we find with this run instruction and re-adding it so
        // that it gets a new geom similar to how it would when being added to the tab originally
        if let Some(pane_id) = self
            .panes
            .iter()
            .find_map(|(pid, p)| {
                if p.invoked_with() == &run {
                    Some(pid)
                } else {
                    None
                }
            })
            .copied()
        {
            if let Some(mut pane) = self.panes.remove(&pane_id) {
                // we must strip the logical position here because it's likely a straggler from
                // this pane's previous tab and would cause chaos if considered in the new one
                let mut pane_geom = pane.position_and_size();
                pane_geom.logical_position = None;
                pane.set_geom(pane_geom);
                self.add_pane_with_existing_geom(pane.pid(), pane);
            }
        }
    }
    pub fn add_pane_to_stack(&mut self, pane_id_in_stack: &PaneId, mut pane: Box<dyn Pane>) {
        let mut pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        match pane_grid.make_room_in_stack_of_pane_id_for_pane(pane_id_in_stack) {
            Ok(new_pane_geom) => {
                pane.set_geom(new_pane_geom);
                self.panes.insert(pane.pid(), pane);
                self.set_force_render(); // TODO: why do we need this?
                return;
            },
            Err(_e) => {
                let should_relayout = false;
                return self.add_pane_without_stacked_resize(pane.pid(), pane, should_relayout);
            },
        }
    }
    fn add_pane(
        &mut self,
        pane_id: PaneId,
        pane: Box<dyn Pane>,
        should_relayout: bool,
        client_id: Option<ClientId>,
    ) {
        if self.panes.is_empty() {
            self.panes.insert(pane_id, pane);
            return;
        }
        let stacked_resize = { *self.stacked_resize.borrow() };

        if let Some(client_id) = client_id {
            if stacked_resize && self.is_connected(&client_id) {
                self.add_pane_with_stacked_resize(pane_id, pane, should_relayout, client_id);
                return;
            }
        }
        self.add_pane_without_stacked_resize(pane_id, pane, should_relayout)
    }
    fn add_pane_without_stacked_resize(
        &mut self,
        pane_id: PaneId,
        mut pane: Box<dyn Pane>,
        should_relayout: bool,
    ) {
        let cursor_height_width_ratio = self.cursor_height_width_ratio();
        let mut pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        let pane_id_and_split_direction =
            pane_grid.find_room_for_new_pane(cursor_height_width_ratio);
        match pane_id_and_split_direction {
            Some((pane_id_to_split, split_direction)) => {
                // this unwrap is safe because floating panes should not be visible if there are no floating panes
                let pane_to_split = self.panes.get_mut(&pane_id_to_split).unwrap();
                let size_of_both_panes = pane_to_split.position_and_size();
                if let Some((first_geom, second_geom)) = split(split_direction, &size_of_both_panes)
                {
                    pane_to_split.set_geom(first_geom);
                    pane.set_geom(second_geom);
                    self.panes.insert(pane_id, pane);
                    if should_relayout {
                        self.relayout(!split_direction);
                    }
                }
            },
            None => {
                // we couldn't add the pane normally, let's see if there's room in one of the
                // stacks...
                match pane_grid.make_room_in_stack_for_pane() {
                    Ok(new_pane_geom) => {
                        pane.set_geom(new_pane_geom);
                        self.panes.insert(pane_id, pane); // TODO: is set_geom the right one?
                    },
                    Err(e) => {
                        log::error!("Failed to add pane to stack: {:?}", e);
                    },
                }
            },
        }
    }
    fn add_pane_with_stacked_resize(
        &mut self,
        pane_id: PaneId,
        mut pane: Box<dyn Pane>,
        should_relayout: bool,
        client_id: ClientId,
    ) {
        let cursor_height_width_ratio = self.cursor_height_width_ratio();
        let mut pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        let Some(active_pane_id) = self.active_panes.get(&client_id) else {
            log::error!("Could not find active pane id for client_id");
            return;
        };
        if pane_grid
            .get_pane_geom(active_pane_id)
            .map(|p| p.is_stacked())
            .unwrap_or(false)
        {
            // try to add the pane to the stack of the active pane
            match pane_grid.make_room_in_stack_of_pane_id_for_pane(active_pane_id) {
                Ok(new_pane_geom) => {
                    pane.set_geom(new_pane_geom);
                    self.panes.insert(pane_id, pane);
                    self.set_force_render(); // TODO: why do we need this?
                    return;
                },
                Err(_e) => {
                    return self.add_pane_without_stacked_resize(pane_id, pane, should_relayout);
                },
            }
        }
        let pane_id_and_split_direction =
            pane_grid.split_pane(active_pane_id, cursor_height_width_ratio);
        match pane_id_and_split_direction {
            Some((pane_id_to_split, split_direction)) => {
                // this unwrap is safe because floating panes should not be visible if there are no floating panes
                let pane_to_split = self.panes.get_mut(&pane_id_to_split).unwrap();
                let size_of_both_panes = pane_to_split.position_and_size();
                if let Some((first_geom, second_geom)) = split(split_direction, &size_of_both_panes)
                {
                    pane_to_split.set_geom(first_geom);
                    pane.set_geom(second_geom);
                    self.panes.insert(pane_id, pane);
                    if should_relayout {
                        self.relayout(!split_direction);
                    }
                }
            },
            None => {
                // we couldn't add the pane normally, let's see if there's room in one of the
                // stacks...
                let _ = pane_grid.make_pane_stacked(active_pane_id);
                match pane_grid.make_room_in_stack_of_pane_id_for_pane(active_pane_id) {
                    Ok(new_pane_geom) => {
                        pane.set_geom(new_pane_geom);
                        self.panes.insert(pane_id, pane); // TODO: is set_geom the right one?
                        return;
                    },
                    Err(_e) => {
                        return self.add_pane_without_stacked_resize(
                            pane_id,
                            pane,
                            should_relayout,
                        );
                    },
                }
            },
        }
    }
    pub fn add_pane_to_stack_of_active_pane(
        &mut self,
        pane_id: PaneId,
        mut pane: Box<dyn Pane>,
        client_id: ClientId,
    ) {
        let mut pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        let Some(active_pane_id) = self.active_panes.get(&client_id) else {
            log::error!("Could not find active pane id for client_id");
            return;
        };
        let pane_id_is_stacked = pane_grid
            .get_pane_geom(active_pane_id)
            .map(|p| p.is_stacked())
            .unwrap_or(false);
        if !pane_id_is_stacked {
            let _ = pane_grid.make_pane_stacked(&active_pane_id);
        }
        match pane_grid.make_room_in_stack_of_pane_id_for_pane(active_pane_id) {
            Ok(new_pane_geom) => {
                pane.set_geom(new_pane_geom);
                self.panes.insert(pane_id, pane);
                self.set_force_render(); // TODO: why do we need this?
                return;
            },
            Err(e) => {
                log::error!("Failed to add pane to stack: {}", e);
            },
        }
    }
    pub fn add_pane_to_stack_of_pane_id(
        &mut self,
        pane_id: PaneId,
        mut pane: Box<dyn Pane>,
        root_pane_id: PaneId,
    ) {
        let mut pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        let pane_id_is_stacked = pane_grid
            .get_pane_geom(&root_pane_id)
            .map(|p| p.is_stacked())
            .unwrap_or(false);
        if !pane_id_is_stacked {
            let _ = pane_grid.make_pane_stacked(&root_pane_id);
        }
        match pane_grid.make_room_in_stack_of_pane_id_for_pane(&root_pane_id) {
            Ok(new_pane_geom) => {
                pane.set_geom(new_pane_geom);
                self.panes.insert(pane_id, pane);
                self.set_force_render(); // TODO: why do we need this?
                return;
            },
            Err(e) => {
                log::error!("Failed to add pane to stack: {}", e);
            },
        }
    }
    pub fn fixed_pane_geoms(&self) -> Vec<Viewport> {
        self.panes
            .values()
            .filter_map(|p| {
                let geom = p.position_and_size();
                if geom.cols.is_fixed() || geom.rows.is_fixed() {
                    Some(geom.into())
                } else {
                    None
                }
            })
            .collect()
    }
    pub fn borderless_pane_geoms(&self) -> Vec<Viewport> {
        self.panes
            .values()
            .filter_map(|p| {
                let geom = p.position_and_size();
                if p.borderless() {
                    Some(geom.into())
                } else {
                    None
                }
            })
            .collect()
    }
    pub fn non_selectable_pane_geoms_inside_viewport(&self) -> Vec<Viewport> {
        self.panes
            .values()
            .filter_map(|p| {
                let geom = p.position_and_size();
                if !p.selectable() && is_inside_viewport(&self.viewport.borrow(), p) {
                    Some(geom.into())
                } else {
                    None
                }
            })
            .collect()
    }
    pub fn first_selectable_pane_id(&self) -> Option<PaneId> {
        self.panes
            .iter()
            .filter(|(_id, pane)| pane.selectable())
            .map(|(id, _)| id.to_owned())
            .next()
    }
    pub fn pane_ids(&self) -> impl Iterator<Item = &PaneId> {
        self.panes.keys()
    }
    pub fn relayout(&mut self, direction: SplitDirection) {
        let mut pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        match direction {
            SplitDirection::Horizontal => {
                pane_grid.layout(direction, (*self.display_area.borrow()).cols)
            },
            SplitDirection::Vertical => {
                pane_grid.layout(direction, (*self.display_area.borrow()).rows)
            },
        }
        .or_else(|e| Err(anyError::msg(e)))
        .with_context(|| format!("{:?} relayout of tab failed", direction))
        .non_fatal();

        self.set_pane_frames(self.draw_pane_frames);
    }
    pub fn reapply_pane_frames(&mut self) {
        // same as set_pane_frames except it reapplies the current situation
        self.set_pane_frames(self.draw_pane_frames);
    }
    pub fn set_pane_frames(&mut self, draw_pane_frames: bool) {
        self.draw_pane_frames = draw_pane_frames;
        let viewport = *self.viewport.borrow();
        let position_and_sizes_of_stacks = {
            StackedPanes::new_from_btreemap(&mut self.panes, &self.panes_to_hide)
                .positions_and_sizes_of_all_stacks()
                .unwrap_or_else(|| Default::default())
        };
        for pane in self.panes.values_mut() {
            if !pane.borderless() {
                pane.set_frame(draw_pane_frames);
            }

            #[allow(clippy::if_same_then_else)]
            if draw_pane_frames && !pane.borderless() {
                // there's definitely a frame around this pane, offset its contents
                pane.set_content_offset(Offset::frame(1));
            } else if draw_pane_frames && pane.borderless() {
                // there's no frame around this pane, and the tab isn't handling the boundaries
                // between panes (they each draw their own frames as they please)
                // this one doesn't - do not offset its content
                pane.set_content_offset(Offset::default());
            } else if !is_inside_viewport(&viewport, pane) {
                // this pane is outside the viewport and has no border - it should not have an offset
                pane.set_content_offset(Offset::default());
            } else {
                // no draw_pane_frames and this pane should have a separation to other panes
                // according to its position in the viewport (eg. no separation if its at the
                // viewport bottom) - offset its content accordingly
                let mut position_and_size = pane.current_geom();
                let is_stacked = position_and_size.is_stacked();
                let is_flexible = !position_and_size.rows.is_fixed();
                if let Some(position_and_size_of_stack) = position_and_size
                    .stacked
                    .and_then(|s_id| position_and_sizes_of_stacks.get(&s_id))
                {
                    // we want to check the offset against the position_and_size of the whole
                    // stack rather than the pane, because the stack needs to have a consistent
                    // offset with itself
                    position_and_size = *position_and_size_of_stack;
                };
                let (pane_columns_offset, pane_rows_offset) =
                    pane_content_offset(&position_and_size, &viewport);
                if is_stacked && is_flexible {
                    // 1 to save room for the pane title
                    pane.set_content_offset(Offset::shift_right_top_and_bottom(
                        pane_columns_offset,
                        1,
                        pane_rows_offset,
                    ));
                } else {
                    pane.set_content_offset(Offset::shift(pane_rows_offset, pane_columns_offset));
                }
            }

            resize_pty!(pane, self.os_api, self.senders, self.character_cell_size).unwrap();
        }
        self.reset_boundaries();
    }
    pub fn can_split_pane_horizontally(&mut self, client_id: ClientId) -> bool {
        if let Some(active_pane_id) = &self.active_panes.get(&client_id) {
            if let Some(active_pane) = self.panes.get_mut(active_pane_id) {
                let mut full_pane_size = active_pane.position_and_size();

                if full_pane_size.is_stacked() {
                    let Some(position_and_size_of_stack) =
                        StackedPanes::new_from_btreemap(&mut self.panes, &self.panes_to_hide)
                            .position_and_size_of_stack(&active_pane_id)
                    else {
                        log::error!("Failed to find position and size of stack");
                        return false;
                    };
                    full_pane_size = position_and_size_of_stack;
                }

                if full_pane_size.rows.as_usize() < MIN_TERMINAL_HEIGHT * 2 {
                    return false;
                } else {
                    return split(SplitDirection::Horizontal, &full_pane_size).is_some();
                }
            }
        }
        false
    }
    pub fn can_split_pane_vertically(&mut self, client_id: ClientId) -> bool {
        if let Some(active_pane_id) = &self.active_panes.get(&client_id) {
            if let Some(active_pane) = self.panes.get_mut(active_pane_id) {
                let mut full_pane_size = active_pane.position_and_size();

                if full_pane_size.is_stacked() {
                    let Some(position_and_size_of_stack) =
                        StackedPanes::new_from_btreemap(&mut self.panes, &self.panes_to_hide)
                            .position_and_size_of_stack(&active_pane_id)
                    else {
                        log::error!("Failed to find position and size of stack");
                        return false;
                    };
                    full_pane_size = position_and_size_of_stack;
                }

                if full_pane_size.cols.as_usize() < MIN_TERMINAL_WIDTH * 2 {
                    return false;
                }
                return split(SplitDirection::Vertical, &full_pane_size).is_some();
            }
        }
        false
    }
    pub fn split_pane_horizontally(
        &mut self,
        pid: PaneId,
        mut new_pane: Box<dyn Pane>,
        client_id: ClientId,
    ) {
        let active_pane_id = &self.active_panes.get(&client_id).unwrap();
        let mut full_pane_size = self
            .panes
            .get(active_pane_id)
            .map(|p| p.position_and_size())
            .unwrap();
        if full_pane_size.is_stacked() {
            match StackedPanes::new_from_btreemap(&mut self.panes, &self.panes_to_hide)
                .position_and_size_of_stack(&active_pane_id)
            {
                Some(position_and_size_of_stack) => {
                    full_pane_size = position_and_size_of_stack;
                },
                None => {
                    log::error!("Failed to find position and size of stack");
                },
            }
        }
        let active_pane = self.panes.get_mut(active_pane_id).unwrap();
        if let Some((top_winsize, bottom_winsize)) =
            split(SplitDirection::Horizontal, &full_pane_size)
        {
            if active_pane.position_and_size().is_stacked() {
                match StackedPanes::new_from_btreemap(&mut self.panes, &self.panes_to_hide)
                    .resize_panes_in_stack(&active_pane_id, top_winsize)
                {
                    Ok(_) => {},
                    Err(e) => {
                        log::error!("Failed to resize stack: {}", e);
                    },
                }
            } else {
                active_pane.set_geom(top_winsize);
            }
            new_pane.set_geom(bottom_winsize);
            self.panes.insert(pid, new_pane);
            self.relayout(SplitDirection::Vertical);
        }
    }
    pub fn split_pane_vertically(
        &mut self,
        pid: PaneId,
        mut new_pane: Box<dyn Pane>,
        client_id: ClientId,
    ) {
        let active_pane_id = &self.active_panes.get(&client_id).unwrap();
        let mut full_pane_size = self
            .panes
            .get(active_pane_id)
            .map(|p| p.position_and_size())
            .unwrap();
        if full_pane_size.is_stacked() {
            match StackedPanes::new_from_btreemap(&mut self.panes, &self.panes_to_hide)
                .position_and_size_of_stack(&active_pane_id)
            {
                Some(position_and_size_of_stack) => {
                    full_pane_size = position_and_size_of_stack;
                },
                None => {
                    log::error!("Failed to find position and size of stack");
                },
            }
        }
        let active_pane = self.panes.get_mut(active_pane_id).unwrap();
        if let Some((left_winsize, right_winsize)) =
            split(SplitDirection::Vertical, &full_pane_size)
        {
            if active_pane.position_and_size().is_stacked() {
                match StackedPanes::new_from_btreemap(&mut self.panes, &self.panes_to_hide)
                    .resize_panes_in_stack(&active_pane_id, left_winsize)
                {
                    Ok(_) => {},
                    Err(e) => {
                        log::error!("Failed to resize stack: {}", e);
                    },
                }
            } else {
                active_pane.set_geom(left_winsize);
            }
            new_pane.set_geom(right_winsize);
            self.panes.insert(pid, new_pane);
            self.relayout(SplitDirection::Horizontal);
        }
    }
    pub fn focus_pane_for_all_clients(&mut self, pane_id: PaneId) {
        let connected_clients: Vec<ClientId> =
            self.connected_clients.borrow().iter().copied().collect();
        for client_id in connected_clients {
            if self
                .panes
                .get(&pane_id)
                .map(|p| p.current_geom().is_stacked())
                .unwrap_or(false)
            {
                let _ = StackedPanes::new_from_btreemap(&mut self.panes, &self.panes_to_hide)
                    .expand_pane(&pane_id);
            }
            self.active_panes
                .insert(client_id, pane_id, &mut self.panes);
            self.set_pane_active_at(pane_id);
        }
        self.set_force_render();
        self.reapply_pane_frames();
    }
    pub fn pane_ids_in_stack_of_pane_id(&mut self, pane_id: &PaneId) -> Vec<PaneId> {
        if let Some(stack_id) = self
            .panes
            .get(pane_id)
            .and_then(|p| p.position_and_size().stacked)
        {
            StackedPanes::new_from_btreemap(&mut self.panes, &self.panes_to_hide)
                .pane_ids_in_stack(stack_id)
        } else {
            vec![]
        }
    }
    pub fn focus_pane_for_all_clients_in_stack(&mut self, pane_id: PaneId, stack_id: usize) {
        let connected_clients: Vec<ClientId> =
            self.connected_clients.borrow().iter().copied().collect();
        let pane_ids_in_stack = {
            StackedPanes::new_from_btreemap(&mut self.panes, &self.panes_to_hide)
                .pane_ids_in_stack(stack_id)
        };
        if self
            .panes
            .get(&pane_id)
            .map(|p| p.current_geom().is_stacked())
            .unwrap_or(false)
        {
            let _ = StackedPanes::new_from_btreemap(&mut self.panes, &self.panes_to_hide)
                .expand_pane(&pane_id);
        }
        for client_id in connected_clients {
            if self
                .active_panes
                .get(&client_id)
                .map(|p_id| pane_ids_in_stack.contains(p_id))
                .unwrap_or(false)
            {
                self.active_panes
                    .insert(client_id, pane_id, &mut self.panes);
                self.set_pane_active_at(pane_id);
            }
        }
        self.set_force_render();
        self.reapply_pane_frames();
    }
    pub fn reapply_pane_focus(&mut self) {
        let connected_clients: Vec<ClientId> =
            self.connected_clients.borrow().iter().copied().collect();
        let mut stack_ids_to_pane_ids_to_expand = vec![];
        for client_id in connected_clients {
            match &self.active_panes.get(&client_id).copied() {
                Some(pane_id) => {
                    if let Some(stack_id) = self
                        .panes
                        .get(&pane_id)
                        .and_then(|p| p.current_geom().stacked)
                    {
                        stack_ids_to_pane_ids_to_expand.push((stack_id, *pane_id));
                    }
                    self.active_panes
                        .insert(client_id, *pane_id, &mut self.panes);
                    self.set_pane_active_at(*pane_id);
                },
                None => {
                    if let Some(first_pane_id) = self.first_selectable_pane_id() {
                        let pane_id = first_pane_id;
                        if let Some(stack_id) = self
                            .panes
                            .get(&pane_id)
                            .and_then(|p| p.current_geom().stacked)
                        {
                            stack_ids_to_pane_ids_to_expand.push((stack_id, pane_id));
                        }
                        self.active_panes
                            .insert(client_id, pane_id, &mut self.panes);
                        self.set_pane_active_at(pane_id);
                    }
                },
            }
        }
        for (stack_id, pane_id) in stack_ids_to_pane_ids_to_expand {
            let _ = StackedPanes::new_from_btreemap(&mut self.panes, &self.panes_to_hide)
                .expand_pane(&pane_id);
            self.focus_pane_for_all_clients_in_stack(pane_id, stack_id);
        }
        self.set_force_render();
        self.reapply_pane_frames();
    }
    pub fn expand_pane_in_stack(&mut self, pane_id: PaneId) -> Vec<PaneId> {
        // returns all pane ids in stack
        match StackedPanes::new_from_btreemap(&mut self.panes, &self.panes_to_hide)
            .expand_pane(&pane_id)
        {
            Ok(all_panes_in_stack) => {
                let connected_clients: Vec<ClientId> =
                    self.connected_clients.borrow().iter().copied().collect();
                for client_id in connected_clients {
                    if let Some(focused_pane_id_for_client) = self.active_panes.get(&client_id) {
                        if all_panes_in_stack.contains(focused_pane_id_for_client) {
                            self.active_panes
                                .insert(client_id, pane_id, &mut self.panes);
                            self.set_pane_active_at(pane_id);
                        }
                    }
                }
                self.set_force_render();
                self.reapply_pane_frames();
                all_panes_in_stack
            },
            Err(e) => {
                log::error!("Failed to expand pane in stack: {:?}", e);
                vec![]
            },
        }
    }
    pub fn focus_pane(&mut self, pane_id: PaneId, client_id: ClientId) {
        let pane_is_selectable = self
            .panes
            .get(&pane_id)
            .map(|p| p.selectable())
            .unwrap_or(false);
        if !pane_is_selectable {
            log::error!("Cannot focus pane {:?} as it is not selectable!", pane_id);
            return;
        }
        if self.panes_to_hide.contains(&pane_id) {
            // this means there is a fullscreen pane that is not the current pane, let's unset it
            // before changing focus
            self.unset_fullscreen();
        }
        if let Some(stack_id) = self
            .panes
            .get(&pane_id)
            .and_then(|p| p.current_geom().stacked)
        {
            let _ = StackedPanes::new_from_btreemap(&mut self.panes, &self.panes_to_hide)
                .expand_pane(&pane_id);
            self.focus_pane_for_all_clients_in_stack(pane_id, stack_id);
            self.reapply_pane_frames();
        }
        self.active_panes
            .insert(client_id, pane_id, &mut self.panes);
        if self.session_is_mirrored {
            // move all clients
            let connected_clients: Vec<ClientId> =
                self.connected_clients.borrow().iter().copied().collect();
            for client_id in connected_clients {
                self.active_panes
                    .insert(client_id, pane_id, &mut self.panes);
            }
        }
        self.reset_boundaries();
    }
    pub fn focus_pane_if_exists(&mut self, pane_id: PaneId, client_id: ClientId) -> Result<()> {
        if self.panes.get(&pane_id).is_some() {
            self.focus_pane(pane_id, client_id);
            Ok(())
        } else {
            Err(anyhow!("Pane not found"))
        }
    }
    pub fn focus_pane_at_position(&mut self, position_and_size: PaneGeom, client_id: ClientId) {
        if let Some(pane_id) = self
            .panes
            .iter()
            .find(|(_pid, pane)| pane.position_and_size() == position_and_size)
            .map(|(pid, _p)| *pid)
        {
            if let Some(currently_active_pane_id) = self.active_panes.get(&client_id) {
                let prev_geom = {
                    if let Some(currently_focused_pane) =
                        self.panes.get_mut(currently_active_pane_id)
                    {
                        let prev_geom = currently_focused_pane.position_and_size();
                        currently_focused_pane.set_geom(position_and_size);
                        Some(prev_geom)
                    } else {
                        None
                    }
                };
                if let Some(prev_geom) = prev_geom {
                    if let Some(previous_pane) = self.panes.get_mut(&pane_id) {
                        previous_pane.set_geom(prev_geom);
                        self.reset_boundaries();
                    }
                }
            }
        }
    }
    pub fn focus_pane_if_client_not_focused(&mut self, pane_id: PaneId, client_id: ClientId) {
        match self.active_panes.get(&client_id) {
            Some(already_focused_pane_id) => self.focus_pane(*already_focused_pane_id, client_id),
            None => self.focus_pane(pane_id, client_id),
        }
    }
    pub fn clear_active_panes(&mut self) {
        self.active_panes.clear(&mut self.panes);
        self.reset_boundaries();
    }
    pub fn first_active_pane_id(&self) -> Option<PaneId> {
        self.connected_clients
            .borrow()
            .iter()
            .next()
            .and_then(|first_client_id| self.active_panes.get(first_client_id).copied())
    }
    pub fn focused_pane_id(&self, client_id: ClientId) -> Option<PaneId> {
        self.active_panes.get(&client_id).copied()
    }
    #[allow(clippy::borrowed_box)]
    pub fn get_pane(&self, pane_id: PaneId) -> Option<&Box<dyn Pane>> {
        self.panes.get(&pane_id)
    }
    pub fn get_pane_mut(&mut self, pane_id: PaneId) -> Option<&mut Box<dyn Pane>> {
        self.panes.get_mut(&pane_id)
    }
    pub fn get_active_pane_id(&self, client_id: ClientId) -> Option<PaneId> {
        self.active_panes.get(&client_id).copied()
    }
    pub fn panes_contain(&self, pane_id: &PaneId) -> bool {
        self.panes.contains_key(pane_id)
    }
    pub fn set_force_render(&mut self) {
        for pane in self.panes.values_mut() {
            pane.set_should_render(true);
            pane.set_should_render_boundaries(true);
            pane.render_full_viewport();
        }
        self.reset_boundaries();
    }
    pub fn has_active_panes(&self) -> bool {
        !self.active_panes.is_empty()
    }
    pub fn has_panes(&self) -> bool {
        !self.panes.is_empty()
    }
    pub fn render(
        &mut self,
        output: &mut Output,
        floating_panes_are_visible: bool,
        mouse_hover_pane_id: &HashMap<ClientId, PaneId>,
        current_pane_group: HashMap<ClientId, Vec<PaneId>>,
    ) -> Result<()> {
        let err_context = || "failed to render tiled panes";

        let connected_clients: Vec<ClientId> =
            { self.connected_clients.borrow().iter().copied().collect() };
        let multiple_users_exist_in_session = { self.connected_clients_in_app.borrow().len() > 1 };
        let mut client_id_to_boundaries: HashMap<ClientId, Boundaries> = HashMap::new();
        let active_panes = if floating_panes_are_visible {
            HashMap::new()
        } else {
            self.active_panes
                .iter()
                .filter(|(client_id, _pane_id)| connected_clients.contains(client_id))
                .map(|(client_id, pane_id)| (*client_id, *pane_id))
                .collect()
        };
        let (stacked_pane_ids_under_flexible_pane, stacked_pane_ids_over_flexible_pane) = {
            StackedPanes::new_from_btreemap(&mut self.panes, &self.panes_to_hide)
                .stacked_pane_ids_under_and_over_flexible_panes()
                .with_context(err_context)?
        };
        let (stacked_pane_ids_on_top_of_stacks, stacked_pane_ids_on_bottom_of_stacks) = {
            StackedPanes::new_from_btreemap(&mut self.panes, &self.panes_to_hide)
                .stacked_pane_ids_on_top_and_bottom_of_stacks()
                .with_context(err_context)?
        };
        for (kind, pane) in self.panes.iter_mut() {
            if !self.panes_to_hide.contains(&pane.pid()) {
                let pane_is_stacked_under =
                    stacked_pane_ids_under_flexible_pane.contains(&pane.pid());
                let pane_is_stacked_over =
                    stacked_pane_ids_over_flexible_pane.contains(&pane.pid());
                let pane_is_on_top_of_stack =
                    stacked_pane_ids_on_top_of_stacks.contains(&pane.pid());
                let pane_is_on_bottom_of_stack =
                    stacked_pane_ids_on_bottom_of_stacks.contains(&pane.pid());
                let should_draw_pane_frames = self.draw_pane_frames;
                let pane_is_stacked = pane.current_geom().is_stacked();
                let pane_is_one_liner_in_stack =
                    pane_is_stacked && pane.current_geom().rows.is_fixed();
                let pane_is_selectable = pane.selectable();
                let mut pane_contents_and_ui = PaneContentsAndUi::new(
                    pane,
                    output,
                    self.style,
                    &active_panes,
                    multiple_users_exist_in_session,
                    None,
                    pane_is_stacked_under,
                    pane_is_stacked_over,
                    should_draw_pane_frames,
                    &mouse_hover_pane_id,
                    current_pane_group.clone(),
                );
                for client_id in &connected_clients {
                    let client_mode = self
                        .mode_info
                        .borrow()
                        .get(client_id)
                        .unwrap_or(&self.default_mode_info)
                        .mode;
                    let err_context =
                        || format!("failed to render tiled panes for client {client_id}");
                    if let PaneId::Plugin(..) = kind {
                        if !pane_is_one_liner_in_stack {
                            pane_contents_and_ui
                                .render_pane_contents_for_client(*client_id)
                                .with_context(err_context)?;
                        }
                    }
                    let is_floating = false;
                    if self.draw_pane_frames {
                        pane_contents_and_ui
                            .render_pane_frame(
                                *client_id,
                                client_mode,
                                self.session_is_mirrored,
                                is_floating,
                                pane_is_selectable,
                            )
                            .with_context(err_context)?;
                    } else if pane_is_stacked {
                        // if we have no pane frames but the pane is stacked, we need to render its
                        // frame which will amount to only rendering the title line
                        pane_contents_and_ui
                            .render_pane_frame(
                                *client_id,
                                client_mode,
                                self.session_is_mirrored,
                                is_floating,
                                pane_is_selectable,
                            )
                            .with_context(err_context)?;
                        // we also need to render its boundaries as normal
                        let boundaries = client_id_to_boundaries
                            .entry(*client_id)
                            .or_insert_with(|| Boundaries::new(*self.viewport.borrow()));
                        pane_contents_and_ui.render_pane_boundaries(
                            *client_id,
                            client_mode,
                            boundaries,
                            self.session_is_mirrored,
                            pane_is_on_top_of_stack,
                            pane_is_on_bottom_of_stack,
                        );
                    } else {
                        let boundaries = client_id_to_boundaries
                            .entry(*client_id)
                            .or_insert_with(|| Boundaries::new(*self.viewport.borrow()));
                        pane_contents_and_ui.render_pane_boundaries(
                            *client_id,
                            client_mode,
                            boundaries,
                            self.session_is_mirrored,
                            pane_is_on_top_of_stack,
                            pane_is_on_bottom_of_stack,
                        );
                    }
                    pane_contents_and_ui.render_terminal_title_if_needed(
                        *client_id,
                        client_mode,
                        &mut self.window_title,
                    );
                    // this is done for panes that don't have their own cursor (eg. panes of
                    // another user)
                    pane_contents_and_ui
                        .render_fake_cursor_if_needed(*client_id)
                        .with_context(err_context)?;
                }
                if let PaneId::Terminal(..) = kind {
                    if !pane_is_one_liner_in_stack {
                        pane_contents_and_ui
                            .render_pane_contents_to_multiple_clients(
                                connected_clients.iter().copied(),
                            )
                            .with_context(err_context)?;
                    }
                }
            }
        }
        // render boundaries if needed
        for (client_id, boundaries) in client_id_to_boundaries {
            let boundaries_to_render = boundaries
                .render(self.client_id_to_boundaries.get(&client_id))
                .with_context(err_context)?;
            self.client_id_to_boundaries.insert(client_id, boundaries);
            output
                .add_character_chunks_to_client(client_id, boundaries_to_render, None)
                .with_context(err_context)?;
        }
        if floating_panes_are_visible {
            // we do this here so that when they are toggled off, we will make sure to re-render the title
            self.window_title = None;
        }
        Ok(())
    }
    pub fn get_panes(&self) -> impl Iterator<Item = (&PaneId, &Box<dyn Pane>)> {
        self.panes.iter()
    }
    pub fn set_geom_for_pane_with_run(
        &mut self,
        run: Option<Run>,
        geom: PaneGeom,
        borderless: bool,
    ) {
        match self
            .panes
            .iter_mut()
            .find(|(_, p)| p.invoked_with() == &run)
        {
            Some((_, pane)) => {
                pane.set_geom(geom);
                pane.set_borderless(borderless);
                if self.draw_pane_frames {
                    pane.set_content_offset(Offset::frame(1));
                }
            },
            None => {
                log::error!("Failed to find pane with run: {:?}", run);
            },
        }
    }
    pub fn set_geom_for_pane_with_id(&mut self, pane_id: &PaneId, geom: PaneGeom) {
        match self.panes.get_mut(pane_id) {
            Some(pane) => {
                pane.set_geom(geom);
            },
            None => {
                log::error!("Failed to find pane with id: {:?}", pane_id);
            },
        }
    }
    fn display_area_changed(&self, new_screen_size: Size) -> bool {
        let display_area = self.display_area.borrow();
        new_screen_size.rows != display_area.rows || new_screen_size.cols != display_area.cols
    }
    pub fn resize(&mut self, new_screen_size: Size) {
        // this is blocked out to appease the borrow checker
        {
            if self.display_area_changed(new_screen_size) {
                self.clear_tombstones();
            }
            let mut display_area = self.display_area.borrow_mut();
            let mut viewport = self.viewport.borrow_mut();
            let Size { rows, cols } = new_screen_size;
            let mut pane_grid = TiledPaneGrid::new(
                &mut self.panes,
                &self.panes_to_hide,
                *display_area,
                *viewport,
            );
            match pane_grid.layout(SplitDirection::Horizontal, cols) {
                Ok(_) => {
                    let column_difference = cols as isize - display_area.cols as isize;
                    // FIXME: Should the viewport be an Offset?
                    viewport.cols = (viewport.cols as isize + column_difference) as usize;
                    display_area.cols = cols;
                },
                Err(e) => match e.downcast_ref::<ZellijError>() {
                    Some(ZellijError::PaneSizeUnchanged) => {}, // ignore unchanged layout
                    _ => {
                        // display area still changed, even if we had an error
                        display_area.cols = cols;
                        Err::<(), _>(anyError::msg(e))
                            .context("failed to resize tab horizontally")
                            .non_fatal();
                    },
                },
            };
            match pane_grid.layout(SplitDirection::Vertical, rows) {
                Ok(_) => {
                    let row_difference = rows as isize - display_area.rows as isize;
                    viewport.rows = (viewport.rows as isize + row_difference) as usize;
                    display_area.rows = rows;
                },
                Err(e) => match e.downcast_ref::<ZellijError>() {
                    Some(ZellijError::PaneSizeUnchanged) => {}, // ignore unchanged layout
                    _ => {
                        // display area still changed, even if we had an error
                        display_area.rows = rows;
                        Err::<(), _>(anyError::msg(e))
                            .context("failed to resize tab vertically")
                            .non_fatal();
                    },
                },
            };
        }
        self.set_pane_frames(self.draw_pane_frames);
    }

    pub fn resize_active_pane(
        &mut self,
        client_id: ClientId,
        strategy: &ResizeStrategy,
    ) -> Result<()> {
        if *self.stacked_resize.borrow() && strategy.direction.is_none() {
            if let Some(active_pane_id) = self.get_active_pane_id(client_id) {
                self.stacked_resize_pane_with_id(active_pane_id, strategy)?;
                self.reapply_pane_frames();
            }
        } else {
            if let Some(active_pane_id) = self.get_active_pane_id(client_id) {
                self.resize_pane_with_id(*strategy, active_pane_id, None)?;
            }
        }

        Ok(())
    }
    fn resize_or_stack_pane_up(&mut self, pane_id: PaneId, resize_percent: (f64, f64)) -> bool {
        // true - successfully resized
        let mut strategy = ResizeStrategy::new(Resize::Increase, Some(Direction::Up));
        strategy.invert_on_boundaries = false;
        let successfully_resized_up =
            match self.resize_pane_with_id(strategy, pane_id, Some(resize_percent)) {
                Ok(_) => true,
                Err(_) => false,
            };
        if successfully_resized_up {
            return true;
        } else {
            let mut pane_grid = TiledPaneGrid::new(
                &mut self.panes,
                &self.panes_to_hide,
                *self.display_area.borrow(),
                *self.viewport.borrow(),
            );
            if let Some(pane_ids_to_resize) = pane_grid.stack_pane_up(&pane_id) {
                for pane_id in pane_ids_to_resize {
                    if let Some(pane) = self.panes.get_mut(&pane_id) {
                        let _ =
                            resize_pty!(pane, self.os_api, self.senders, self.character_cell_size);
                    }
                }
                return true;
            }
        }
        false
    }
    fn resize_or_stack_pane_down(&mut self, pane_id: PaneId, resize_percent: (f64, f64)) -> bool {
        // true - successfully resized
        let mut strategy = ResizeStrategy::new(Resize::Increase, Some(Direction::Down));
        strategy.invert_on_boundaries = false;
        let successfully_resized_up =
            match self.resize_pane_with_id(strategy, pane_id, Some(resize_percent)) {
                Ok(_) => true,
                Err(_) => false,
            };
        if successfully_resized_up {
            return true;
        } else {
            let mut pane_grid = TiledPaneGrid::new(
                &mut self.panes,
                &self.panes_to_hide,
                *self.display_area.borrow(),
                *self.viewport.borrow(),
            );
            if let Some(pane_ids_to_resize) = pane_grid.stack_pane_down(&pane_id) {
                for pane_id in pane_ids_to_resize {
                    if let Some(pane) = self.panes.get_mut(&pane_id) {
                        let _ =
                            resize_pty!(pane, self.os_api, self.senders, self.character_cell_size);
                    }
                }
                return true;
            }
        }
        false
    }
    fn resize_or_stack_pane_left(&mut self, pane_id: PaneId, resize_percent: (f64, f64)) -> bool {
        // true - successfully resized
        let mut strategy = ResizeStrategy::new(Resize::Increase, Some(Direction::Left));
        strategy.invert_on_boundaries = false;
        let successfully_resized_up =
            match self.resize_pane_with_id(strategy, pane_id, Some(resize_percent)) {
                Ok(_) => true,
                Err(_) => false,
            };
        if successfully_resized_up {
            return true;
        } else {
            let mut pane_grid = TiledPaneGrid::new(
                &mut self.panes,
                &self.panes_to_hide,
                *self.display_area.borrow(),
                *self.viewport.borrow(),
            );
            if let Some(pane_ids_to_resize) = pane_grid.stack_pane_left(&pane_id) {
                for pane_id in pane_ids_to_resize {
                    if let Some(pane) = self.panes.get_mut(&pane_id) {
                        let _ =
                            resize_pty!(pane, self.os_api, self.senders, self.character_cell_size);
                    }
                }
                return true;
            }
        }
        false
    }
    fn resize_or_stack_pane_right(&mut self, pane_id: PaneId, resize_percent: (f64, f64)) -> bool {
        // true - successfully resized
        let mut strategy = ResizeStrategy::new(Resize::Increase, Some(Direction::Right));
        strategy.invert_on_boundaries = false;
        let successfully_resized_up =
            match self.resize_pane_with_id(strategy, pane_id, Some(resize_percent)) {
                Ok(_) => true,
                Err(_) => false,
            };
        if successfully_resized_up {
            return true;
        } else {
            let mut pane_grid = TiledPaneGrid::new(
                &mut self.panes,
                &self.panes_to_hide,
                *self.display_area.borrow(),
                *self.viewport.borrow(),
            );
            if let Some(pane_ids_to_resize) = pane_grid.stack_pane_right(&pane_id) {
                for pane_id in pane_ids_to_resize {
                    if let Some(pane) = self.panes.get_mut(&pane_id) {
                        let _ =
                            resize_pty!(pane, self.os_api, self.senders, self.character_cell_size);
                    }
                }
                return true;
            }
        }
        false
    }
    fn update_tombstones_before_increase(
        &mut self,
        focused_pane_id: PaneId,
        pane_state: HashMap<PaneId, PaneGeom>,
    ) {
        match self.tombstones_before_increase.as_mut() {
            Some((focused_pane_id_in_tombstone, pane_geoms)) => {
                let last_state = pane_geoms.last();
                let last_state_has_same_pane_count = last_state
                    .map(|last_state| last_state.len() == pane_state.len())
                    .unwrap_or(false);
                let last_state_equals_current_state = last_state
                    .map(|last_state| last_state == &pane_state)
                    .unwrap_or(false);
                if last_state_equals_current_state {
                    return;
                }
                if *focused_pane_id_in_tombstone == focused_pane_id
                    && last_state_has_same_pane_count
                {
                    pane_geoms.push(pane_state);
                } else {
                    self.clear_tombstones();
                    self.tombstones_before_increase = Some((focused_pane_id, vec![pane_state]));
                }
            },
            None => {
                self.clear_tombstones();
                self.tombstones_before_increase = Some((focused_pane_id, vec![pane_state]));
            },
        }
    }
    fn update_tombstones_before_decrease(
        &mut self,
        focused_pane_id: PaneId,
        pane_state: HashMap<PaneId, PaneGeom>,
    ) {
        match self.tombstones_before_decrease.as_mut() {
            Some((focused_pane_id_in_tombstone, pane_geoms)) => {
                let last_state = pane_geoms.last();
                let last_state_has_same_pane_count = last_state
                    .map(|last_state| last_state.len() == pane_state.len())
                    .unwrap_or(false);
                let last_state_equals_current_state = last_state
                    .map(|last_state| last_state == &pane_state)
                    .unwrap_or(false);
                if last_state_equals_current_state {
                    return;
                }
                if *focused_pane_id_in_tombstone == focused_pane_id
                    && last_state_has_same_pane_count
                {
                    pane_geoms.push(pane_state);
                } else {
                    self.clear_tombstones();
                    self.tombstones_before_decrease = Some((focused_pane_id, vec![pane_state]));
                }
            },
            None => {
                self.clear_tombstones();
                self.tombstones_before_decrease = Some((focused_pane_id, vec![pane_state]));
            },
        }
    }
    fn clear_tombstones(&mut self) {
        self.tombstones_before_increase = None;
        self.tombstones_before_decrease = None;
    }
    fn stacked_resize_pane_with_id(
        &mut self,
        pane_id: PaneId,
        strategy: &ResizeStrategy,
    ) -> Result<bool> {
        let resize_percent = (30.0, 30.0);
        match strategy.resize {
            Resize::Increase => {
                match self.tombstones_before_decrease.as_mut() {
                    Some((tombstone_pane_id, tombstone_pane_state))
                        if *tombstone_pane_id == pane_id =>
                    {
                        if let Some(last_state) = tombstone_pane_state.pop() {
                            if last_state.len() == self.panes.len() {
                                for (pane_id, pane_geom) in last_state {
                                    self.panes
                                        .get_mut(&pane_id)
                                        .map(|pane| pane.set_geom(pane_geom));
                                }
                                self.reapply_pane_frames();
                                return Ok(true);
                            } else {
                                self.tombstones_before_decrease = None;
                            }
                        }
                    },
                    _ => {},
                }

                let mut current_pane_state = HashMap::new();
                for (pane_id, pane) in &self.panes {
                    current_pane_state.insert(*pane_id, pane.current_geom());
                }

                let (
                    direct_neighboring_pane_ids_above,
                    direct_neighboring_pane_ids_below,
                    direct_neighboring_pane_ids_to_the_left,
                    direct_neighboring_pane_ids_to_the_right,
                ) = {
                    let pane_grid = TiledPaneGrid::new(
                        &mut self.panes,
                        &self.panes_to_hide,
                        *self.display_area.borrow(),
                        *self.viewport.borrow(),
                    );
                    (
                        pane_grid.direct_neighboring_pane_ids_above(&pane_id),
                        pane_grid.direct_neighboring_pane_ids_below(&pane_id),
                        pane_grid.direct_neighboring_pane_ids_to_the_left(&pane_id),
                        pane_grid.direct_neighboring_pane_ids_to_the_right(&pane_id),
                    )
                };
                if !direct_neighboring_pane_ids_above.is_empty() {
                    if self.resize_or_stack_pane_up(pane_id, resize_percent) {
                        self.update_tombstones_before_increase(pane_id, current_pane_state);
                        return Ok(true);
                    }
                }
                if !direct_neighboring_pane_ids_below.is_empty() {
                    if self.resize_or_stack_pane_down(pane_id, resize_percent) {
                        self.update_tombstones_before_increase(pane_id, current_pane_state);
                        return Ok(true);
                    }
                }
                if !direct_neighboring_pane_ids_to_the_left.is_empty() {
                    if self.resize_or_stack_pane_left(pane_id, resize_percent) {
                        self.update_tombstones_before_increase(pane_id, current_pane_state);
                        return Ok(true);
                    }
                }
                if !direct_neighboring_pane_ids_to_the_right.is_empty() {
                    if self.resize_or_stack_pane_right(pane_id, resize_percent) {
                        self.update_tombstones_before_increase(pane_id, current_pane_state);
                        return Ok(true);
                    }
                }
                // normal resize if we can't do anything...
                match self.resize_pane_with_id(*strategy, pane_id, None) {
                    Ok(size_changed) => {
                        if size_changed {
                            self.update_tombstones_before_increase(pane_id, current_pane_state);
                        } else {
                            if self.fullscreen_is_active.is_none() {
                                self.toggle_pane_fullscreen(pane_id);
                                return Ok(self.fullscreen_is_active.is_some());
                            } else {
                                return Ok(false);
                            }
                        }
                        return Ok(size_changed);
                    },
                    Err(e) => Err(e),
                }
            },
            Resize::Decrease => {
                if self.fullscreen_is_active.is_some() {
                    self.unset_fullscreen();
                    return Ok(true);
                }

                match self.tombstones_before_increase.as_mut() {
                    Some((tombstone_pane_id, tombstone_pane_state))
                        if *tombstone_pane_id == pane_id =>
                    {
                        if let Some(last_state) = tombstone_pane_state.pop() {
                            if last_state.len() == self.panes.len() {
                                for (pane_id, pane_geom) in last_state {
                                    self.panes
                                        .get_mut(&pane_id)
                                        .map(|pane| pane.set_geom(pane_geom));
                                }
                                self.reapply_pane_frames();
                                return Ok(true);
                            } else {
                                self.tombstones_before_increase = None;
                            }
                        }
                    },
                    _ => {},
                }

                let mut current_pane_state = HashMap::new();
                for (pane_id, pane) in &self.panes {
                    current_pane_state.insert(*pane_id, pane.current_geom());
                }

                let mut pane_grid = TiledPaneGrid::new(
                    &mut self.panes,
                    &self.panes_to_hide,
                    *self.display_area.borrow(),
                    *self.viewport.borrow(),
                );
                if let Some(pane_ids_to_resize) = pane_grid.unstack_pane_up(&pane_id) {
                    for pane_id in pane_ids_to_resize {
                        if let Some(pane) = self.panes.get_mut(&pane_id) {
                            resize_pty!(pane, self.os_api, self.senders, self.character_cell_size)
                                .unwrap();
                        }
                    }
                    self.reapply_pane_frames();
                    self.update_tombstones_before_decrease(pane_id, current_pane_state);
                    Ok(true)
                } else {
                    // normal resize if we were not inside a stack
                    // first try with our custom resize_percent
                    match self.resize_pane_with_id(*strategy, pane_id, Some(resize_percent)) {
                        Ok(pane_size_changed) => {
                            if pane_size_changed {
                                self.update_tombstones_before_decrease(pane_id, current_pane_state);
                                self.reapply_pane_frames();
                                Ok(pane_size_changed)
                            } else {
                                // if it doesn't work, try with the default resize percent
                                match self.resize_pane_with_id(*strategy, pane_id, None) {
                                    Ok(pane_size_changed) => {
                                        if pane_size_changed {
                                            self.update_tombstones_before_decrease(
                                                pane_id,
                                                current_pane_state,
                                            );
                                            self.reapply_pane_frames();
                                        }
                                        Ok(pane_size_changed)
                                    },
                                    Err(e) => Err(e),
                                }
                            }
                        },
                        Err(e) => Err(e),
                    }
                }
            },
        }
    }
    pub fn resize_pane_with_id(
        &mut self,
        strategy: ResizeStrategy,
        pane_id: PaneId,
        resize_percent: Option<(f64, f64)>,
    ) -> Result<bool> {
        let err_context = || format!("failed to resize pand with id: {:?}", pane_id);

        let mut pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );

        let mut pane_size_changed = false;
        match pane_grid
            .change_pane_size(
                &pane_id,
                &strategy,
                resize_percent.unwrap_or((RESIZE_PERCENT, RESIZE_PERCENT)),
            )
            .with_context(err_context)
        {
            Ok(changed) => {
                pane_size_changed = changed;
            },
            Err(err) => match err.downcast_ref::<ZellijError>() {
                Some(ZellijError::PaneSizeUnchanged) => {
                    // try once more with double the resize percent, but let's keep it at that
                    match pane_grid
                        .change_pane_size(
                            &pane_id,
                            &strategy,
                            (RESIZE_PERCENT * 2.0, RESIZE_PERCENT * 2.0),
                        )
                        .with_context(err_context)
                    {
                        Ok(_) => {},
                        Err(err) => match err.downcast_ref::<ZellijError>() {
                            Some(ZellijError::PaneSizeUnchanged) => Err::<(), _>(err).non_fatal(),
                            _ => {
                                return Err(err);
                            },
                        },
                    }
                },
                _ => {
                    return Err(err);
                },
            },
        }

        for pane in self.panes.values_mut() {
            // TODO: only for the panes whose width/height actually changed
            resize_pty!(pane, self.os_api, self.senders, self.character_cell_size).unwrap();
        }
        self.reset_boundaries();
        Ok(pane_size_changed)
    }

    pub fn focus_next_pane(&mut self, client_id: ClientId) {
        let connected_clients: Vec<ClientId> =
            { self.connected_clients.borrow().iter().copied().collect() };
        let active_pane_id = self.get_active_pane_id(client_id).unwrap();
        let next_active_pane_id = {
            let pane_grid = TiledPaneGrid::new(
                &mut self.panes,
                &self.panes_to_hide,
                *self.display_area.borrow(),
                *self.viewport.borrow(),
            );
            pane_grid.next_selectable_pane_id(&active_pane_id)
        };
        if self
            .panes
            .get(&next_active_pane_id)
            .map(|p| p.current_geom().is_stacked())
            .unwrap_or(false)
        {
            let _ = StackedPanes::new_from_btreemap(&mut self.panes, &self.panes_to_hide)
                .expand_pane(&next_active_pane_id);
            self.reapply_pane_frames();
        }

        for client_id in connected_clients {
            self.active_panes
                .insert(client_id, next_active_pane_id, &mut self.panes);
        }
        self.set_pane_active_at(next_active_pane_id);
        self.reset_boundaries();
    }
    pub fn focus_previous_pane(&mut self, client_id: ClientId) {
        let connected_clients: Vec<ClientId> =
            { self.connected_clients.borrow().iter().copied().collect() };
        let active_pane_id = self.get_active_pane_id(client_id).unwrap();
        let next_active_pane_id = {
            let pane_grid = TiledPaneGrid::new(
                &mut self.panes,
                &self.panes_to_hide,
                *self.display_area.borrow(),
                *self.viewport.borrow(),
            );
            pane_grid.previous_selectable_pane_id(&active_pane_id)
        };

        if self
            .panes
            .get(&next_active_pane_id)
            .map(|p| p.current_geom().is_stacked())
            .unwrap_or(false)
        {
            let _ = StackedPanes::new_from_btreemap(&mut self.panes, &self.panes_to_hide)
                .expand_pane(&next_active_pane_id);
            self.reapply_pane_frames();
        }
        for client_id in connected_clients {
            self.active_panes
                .insert(client_id, next_active_pane_id, &mut self.panes);
        }
        self.set_pane_active_at(next_active_pane_id);
        self.reset_boundaries();
    }
    fn set_pane_active_at(&mut self, pane_id: PaneId) {
        if let Some(pane) = self.get_pane_mut(pane_id) {
            pane.set_active_at(Instant::now());
        }
    }
    pub fn cursor_height_width_ratio(&self) -> Option<usize> {
        let character_cell_size = self.character_cell_size.borrow();
        character_cell_size.map(|size_in_pixels| {
            (size_in_pixels.height as f64 / size_in_pixels.width as f64).round() as usize
        })
    }
    pub fn focus_pane_on_edge(&mut self, direction: Direction, client_id: ClientId) {
        let pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        let next_index = pane_grid.pane_id_on_edge(direction).unwrap();
        // render previously active pane so that its frame does not remain actively
        // colored
        let previously_active_pane = self
            .panes
            .get_mut(self.active_panes.get(&client_id).unwrap())
            .unwrap();

        previously_active_pane.set_should_render(true);
        // we render the full viewport to remove any ui elements that might have been
        // there before (eg. another user's cursor)
        previously_active_pane.render_full_viewport();

        let next_active_pane = self.panes.get_mut(&next_index).unwrap();
        next_active_pane.set_should_render(true);
        // we render the full viewport to remove any ui elements that might have been
        // there before (eg. another user's cursor)
        next_active_pane.render_full_viewport();

        self.focus_pane(next_index, client_id);
        self.set_pane_active_at(next_index);
    }
    pub fn move_focus_left(&mut self, client_id: ClientId) -> bool {
        match self.get_active_pane_id(client_id) {
            Some(active_pane_id) => {
                let pane_grid = TiledPaneGrid::new(
                    &mut self.panes,
                    &self.panes_to_hide,
                    *self.display_area.borrow(),
                    *self.viewport.borrow(),
                );
                let next_index = pane_grid.next_selectable_pane_id_to_the_left(&active_pane_id);
                match next_index {
                    Some(p) => {
                        // render previously active pane so that its frame does not remain actively
                        // colored
                        let previously_active_pane = self
                            .panes
                            .get_mut(self.active_panes.get(&client_id).unwrap())
                            .unwrap();

                        previously_active_pane.set_should_render(true);
                        // we render the full viewport to remove any ui elements that might have been
                        // there before (eg. another user's cursor)
                        previously_active_pane.render_full_viewport();

                        let next_active_pane = self.panes.get_mut(&p).unwrap();
                        next_active_pane.set_should_render(true);
                        // we render the full viewport to remove any ui elements that might have been
                        // there before (eg. another user's cursor)
                        next_active_pane.render_full_viewport();

                        self.focus_pane(p, client_id);
                        self.set_pane_active_at(p);

                        true
                    },
                    None => false,
                }
            },
            None => false,
        }
    }
    pub fn move_focus_down(&mut self, client_id: ClientId) -> bool {
        match self.get_active_pane_id(client_id) {
            Some(active_pane_id) => {
                let mut pane_grid = TiledPaneGrid::new(
                    &mut self.panes,
                    &self.panes_to_hide,
                    *self.display_area.borrow(),
                    *self.viewport.borrow(),
                );
                let next_index = pane_grid
                    .next_selectable_pane_id_below(&active_pane_id, false)
                    .or_else(|| pane_grid.progress_stack_down_if_in_stack(&active_pane_id));
                match next_index {
                    Some(p) => {
                        // render previously active pane so that its frame does not remain actively
                        // colored
                        let previously_active_pane = self
                            .panes
                            .get_mut(self.active_panes.get(&client_id).unwrap())
                            .unwrap();

                        previously_active_pane.set_should_render(true);
                        // we render the full viewport to remove any ui elements that might have been
                        // there before (eg. another user's cursor)
                        previously_active_pane.render_full_viewport();

                        let next_active_pane = self.panes.get_mut(&p).unwrap();
                        let next_active_pane_stack_id = next_active_pane.current_geom().stacked;
                        next_active_pane.set_should_render(true);
                        // we render the full viewport to remove any ui elements that might have been
                        // there before (eg. another user's cursor)
                        next_active_pane.render_full_viewport();

                        self.focus_pane(p, client_id);
                        self.set_pane_active_at(p);
                        if let Some(stack_id) = next_active_pane_stack_id {
                            // we do this because a stack pane focus change also changes its
                            // geometry and we need to let the pty know about this (like in a
                            // normal size change)
                            self.focus_pane_for_all_clients_in_stack(p, stack_id);
                            self.reapply_pane_frames();
                        }

                        true
                    },
                    None => false,
                }
            },
            None => false,
        }
    }
    pub fn move_focus_up(&mut self, client_id: ClientId) -> bool {
        match self.get_active_pane_id(client_id) {
            Some(active_pane_id) => {
                let mut pane_grid = TiledPaneGrid::new(
                    &mut self.panes,
                    &self.panes_to_hide,
                    *self.display_area.borrow(),
                    *self.viewport.borrow(),
                );
                let next_index = pane_grid
                    .next_selectable_pane_id_above(&active_pane_id, false)
                    .or_else(|| pane_grid.progress_stack_up_if_in_stack(&active_pane_id));
                match next_index {
                    Some(p) => {
                        // render previously active pane so that its frame does not remain actively
                        // colored
                        let previously_active_pane = self
                            .panes
                            .get_mut(self.active_panes.get(&client_id).unwrap())
                            .unwrap();

                        previously_active_pane.set_should_render(true);
                        // we render the full viewport to remove any ui elements that might have been
                        // there before (eg. another user's cursor)
                        previously_active_pane.render_full_viewport();

                        let next_active_pane = self.panes.get_mut(&p).unwrap();
                        let next_active_pane_stack_id = next_active_pane.current_geom().stacked;
                        next_active_pane.set_should_render(true);
                        // we render the full viewport to remove any ui elements that might have been
                        // there before (eg. another user's cursor)
                        next_active_pane.render_full_viewport();

                        self.focus_pane(p, client_id);
                        self.set_pane_active_at(p);
                        if let Some(stack_id) = next_active_pane_stack_id {
                            // we do this because a stack pane focus change also changes its
                            // geometry and we need to let the pty know about this (like in a
                            // normal size change)
                            self.focus_pane_for_all_clients_in_stack(p, stack_id);
                            self.reapply_pane_frames();
                        }

                        true
                    },
                    None => false,
                }
            },
            None => false,
        }
    }
    pub fn move_focus_right(&mut self, client_id: ClientId) -> bool {
        match self.get_active_pane_id(client_id) {
            Some(active_pane_id) => {
                let pane_grid = TiledPaneGrid::new(
                    &mut self.panes,
                    &self.panes_to_hide,
                    *self.display_area.borrow(),
                    *self.viewport.borrow(),
                );
                let next_index = pane_grid.next_selectable_pane_id_to_the_right(&active_pane_id);
                match next_index {
                    Some(p) => {
                        // render previously active pane so that its frame does not remain actively
                        // colored
                        let previously_active_pane = self
                            .panes
                            .get_mut(self.active_panes.get(&client_id).unwrap())
                            .unwrap();

                        previously_active_pane.set_should_render(true);
                        // we render the full viewport to remove any ui elements that might have been
                        // there before (eg. another user's cursor)
                        previously_active_pane.render_full_viewport();

                        let next_active_pane = self.panes.get_mut(&p).unwrap();
                        next_active_pane.set_should_render(true);
                        // we render the full viewport to remove any ui elements that might have been
                        // there before (eg. another user's cursor)
                        next_active_pane.render_full_viewport();

                        self.focus_pane(p, client_id);
                        self.set_pane_active_at(p);

                        true
                    },
                    None => false,
                }
            },
            None => false,
        }
    }
    pub fn switch_active_pane_with(&mut self, pane_id: PaneId) {
        if let Some(active_pane_id) = self.first_active_pane_id() {
            if let PaneId::Plugin(_) = active_pane_id {
                // we do not implicitly change the location of plugin panes
                // TODO: we might want to make this configurable through a layout property or a
                // plugin API
                return;
            }
            let current_position = self.panes.get(&active_pane_id).unwrap();
            let prev_geom = current_position.position_and_size();
            let prev_geom_override = current_position.geom_override();

            let new_position = self.panes.get_mut(&pane_id).unwrap();
            let next_geom = new_position.position_and_size();
            let next_geom_override = new_position.geom_override();
            new_position.set_geom(prev_geom);
            if let Some(geom) = prev_geom_override {
                new_position.set_geom_override(geom);
            }
            resize_pty!(
                new_position,
                self.os_api,
                self.senders,
                self.character_cell_size
            )
            .unwrap();
            new_position.set_should_render(true);

            let current_position = self.panes.get_mut(&active_pane_id).unwrap();
            current_position.set_geom(next_geom);
            if let Some(geom) = next_geom_override {
                current_position.set_geom_override(geom);
            }
            resize_pty!(
                current_position,
                self.os_api,
                self.senders,
                self.character_cell_size
            )
            .unwrap();
            current_position.set_should_render(true);
            self.focus_pane_for_all_clients(active_pane_id);
            self.set_pane_frames(self.draw_pane_frames);
        }
    }
    pub fn move_active_pane(&mut self, search_backwards: bool, client_id: ClientId) {
        let active_pane_id = self.get_active_pane_id(client_id).unwrap();
        self.move_pane(search_backwards, active_pane_id)
    }
    pub fn move_pane(&mut self, search_backwards: bool, pane_id: PaneId) {
        let new_position_id = {
            let pane_grid = TiledPaneGrid::new(
                &mut self.panes,
                &self.panes_to_hide,
                *self.display_area.borrow(),
                *self.viewport.borrow(),
            );
            if search_backwards {
                pane_grid.previous_selectable_pane_id(&pane_id)
            } else {
                pane_grid.next_selectable_pane_id(&pane_id)
            }
        };
        if self
            .panes
            .get(&new_position_id)
            .map(|p| p.current_geom().is_stacked())
            .unwrap_or(false)
        {
            let _ = StackedPanes::new_from_btreemap(&mut self.panes, &self.panes_to_hide)
                .expand_pane(&new_position_id);
            self.reapply_pane_frames();
        }

        let current_position = self.panes.get(&pane_id).unwrap();
        let prev_geom = current_position.position_and_size();
        let prev_geom_override = current_position.geom_override();

        let new_position = self.panes.get_mut(&new_position_id).unwrap();
        let next_geom = new_position.position_and_size();
        let next_geom_override = new_position.geom_override();
        new_position.set_geom(prev_geom);
        if let Some(geom) = prev_geom_override {
            new_position.set_geom_override(geom);
        }
        resize_pty!(
            new_position,
            self.os_api,
            self.senders,
            self.character_cell_size
        )
        .unwrap();
        new_position.set_should_render(true);

        let current_position = self.panes.get_mut(&pane_id).unwrap();
        current_position.set_geom(next_geom);
        if let Some(geom) = next_geom_override {
            current_position.set_geom_override(geom);
        }
        resize_pty!(
            current_position,
            self.os_api,
            self.senders,
            self.character_cell_size
        )
        .unwrap();
        current_position.set_should_render(true);
        self.reapply_pane_focus();
        self.set_pane_frames(self.draw_pane_frames);
    }
    pub fn move_active_pane_down(&mut self, client_id: ClientId) {
        if let Some(active_pane_id) = self.get_active_pane_id(client_id) {
            self.move_pane_down(active_pane_id);
        }
    }
    pub fn move_pane_down(&mut self, pane_id: PaneId) {
        let mut pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        let next_index = pane_grid
            .next_selectable_pane_id_below(&pane_id, false)
            .or_else(|| pane_grid.progress_stack_down_if_in_stack(&pane_id));
        if let Some(p) = next_index {
            let current_position = self.panes.get(&pane_id).unwrap();
            let prev_geom = current_position.position_and_size();
            let prev_geom_override = current_position.geom_override();

            let new_position = self.panes.get_mut(&p).unwrap();
            let next_geom = new_position.position_and_size();
            let next_geom_override = new_position.geom_override();
            new_position.set_geom(prev_geom);
            if let Some(geom) = prev_geom_override {
                new_position.set_geom_override(geom);
            }
            resize_pty!(
                new_position,
                self.os_api,
                self.senders,
                self.character_cell_size
            )
            .unwrap();
            new_position.set_should_render(true);

            let current_position = self.panes.get_mut(&pane_id).unwrap();
            current_position.set_geom(next_geom);
            if let Some(geom) = next_geom_override {
                current_position.set_geom_override(geom);
            }
            resize_pty!(
                current_position,
                self.os_api,
                self.senders,
                self.character_cell_size
            )
            .unwrap();
            current_position.set_should_render(true);
            self.reapply_pane_focus();
            self.set_pane_frames(self.draw_pane_frames);
        }
    }
    pub fn move_active_pane_left(&mut self, client_id: ClientId) {
        if let Some(active_pane_id) = self.get_active_pane_id(client_id) {
            self.move_pane_left(active_pane_id);
        }
    }
    pub fn move_pane_left(&mut self, pane_id: PaneId) {
        let pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        let next_index = pane_grid.next_selectable_pane_id_to_the_left(&pane_id);
        if let Some(p) = next_index {
            let current_position = self.panes.get(&pane_id).unwrap();
            let prev_geom = current_position.position_and_size();
            let prev_geom_override = current_position.geom_override();

            let new_position = self.panes.get_mut(&p).unwrap();
            let next_geom = new_position.position_and_size();
            let next_geom_override = new_position.geom_override();
            new_position.set_geom(prev_geom);
            if let Some(geom) = prev_geom_override {
                new_position.set_geom_override(geom);
            }
            resize_pty!(
                new_position,
                self.os_api,
                self.senders,
                self.character_cell_size
            )
            .unwrap();
            new_position.set_should_render(true);

            let current_position = self.panes.get_mut(&pane_id).unwrap();
            current_position.set_geom(next_geom);
            if let Some(geom) = next_geom_override {
                current_position.set_geom_override(geom);
            }
            resize_pty!(
                current_position,
                self.os_api,
                self.senders,
                self.character_cell_size
            )
            .unwrap();
            current_position.set_should_render(true);
            self.reapply_pane_focus();
            self.set_pane_frames(self.draw_pane_frames);
        }
    }
    pub fn move_active_pane_right(&mut self, client_id: ClientId) {
        if let Some(active_pane_id) = self.get_active_pane_id(client_id) {
            self.move_pane_right(active_pane_id);
        }
    }
    pub fn move_pane_right(&mut self, pane_id: PaneId) {
        let pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        let next_index = pane_grid.next_selectable_pane_id_to_the_right(&pane_id);
        if let Some(p) = next_index {
            let current_position = self.panes.get(&pane_id).unwrap();
            let prev_geom = current_position.position_and_size();
            let prev_geom_override = current_position.geom_override();

            let new_position = self.panes.get_mut(&p).unwrap();
            let next_geom = new_position.position_and_size();
            let next_geom_override = new_position.geom_override();
            new_position.set_geom(prev_geom);
            if let Some(geom) = prev_geom_override {
                new_position.set_geom_override(geom);
            }
            resize_pty!(
                new_position,
                self.os_api,
                self.senders,
                self.character_cell_size
            )
            .unwrap();
            new_position.set_should_render(true);

            let current_position = self.panes.get_mut(&pane_id).unwrap();
            current_position.set_geom(next_geom);
            if let Some(geom) = next_geom_override {
                current_position.set_geom_override(geom);
            }
            resize_pty!(
                current_position,
                self.os_api,
                self.senders,
                self.character_cell_size
            )
            .unwrap();
            current_position.set_should_render(true);
            self.reapply_pane_focus();
            self.set_pane_frames(self.draw_pane_frames);
        }
    }
    pub fn move_active_pane_up(&mut self, client_id: ClientId) {
        if let Some(active_pane_id) = self.get_active_pane_id(client_id) {
            self.move_pane_up(active_pane_id);
        }
    }
    pub fn move_pane_up(&mut self, pane_id: PaneId) {
        let mut pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        let next_index = pane_grid
            .next_selectable_pane_id_above(&pane_id, false)
            .or_else(|| pane_grid.progress_stack_up_if_in_stack(&pane_id));
        if let Some(p) = next_index {
            let current_position = self.panes.get(&pane_id).unwrap();
            let prev_geom = current_position.position_and_size();
            let prev_geom_override = current_position.geom_override();

            let new_position = self.panes.get_mut(&p).unwrap();
            let next_geom = new_position.position_and_size();
            let next_geom_override = new_position.geom_override();
            new_position.set_geom(prev_geom);
            if let Some(geom) = prev_geom_override {
                new_position.set_geom_override(geom);
            }
            resize_pty!(
                new_position,
                self.os_api,
                self.senders,
                self.character_cell_size
            )
            .unwrap();
            new_position.set_should_render(true);

            let current_position = self.panes.get_mut(&pane_id).unwrap();
            current_position.set_geom(next_geom);
            if let Some(geom) = next_geom_override {
                current_position.set_geom_override(geom);
            }
            resize_pty!(
                current_position,
                self.os_api,
                self.senders,
                self.character_cell_size
            )
            .unwrap();
            current_position.set_should_render(true);
            self.reapply_pane_focus();
            self.set_pane_frames(self.draw_pane_frames);
        }
    }
    pub fn move_clients_out_of_pane(&mut self, pane_id: PaneId) {
        let active_panes: Vec<(ClientId, PaneId)> = self
            .active_panes
            .iter()
            .map(|(cid, pid)| (*cid, *pid))
            .collect();
        let clients_need_to_be_moved = active_panes.iter().any(|(_c_id, p_id)| *p_id == pane_id);
        if !clients_need_to_be_moved {
            return;
        }

        // find the most recently active pane
        let mut next_active_pane_candidates: Vec<(&PaneId, &Box<dyn Pane>)> = self
            .panes
            .iter()
            .filter(|(p_id, p)| !self.panes_to_hide.contains(p_id) && p.selectable())
            .collect();
        next_active_pane_candidates.sort_by(|(_pane_id_a, pane_a), (_pane_id_b, pane_b)| {
            pane_a.active_at().cmp(&pane_b.active_at())
        });
        let next_active_pane_id = next_active_pane_candidates
            .last()
            .map(|(pane_id, _pane)| **pane_id);

        match next_active_pane_id {
            Some(next_active_pane_id) => {
                if self
                    .panes
                    .get(&next_active_pane_id)
                    .map(|p| p.current_geom().is_stacked())
                    .unwrap_or(false)
                {
                    self.expand_pane_in_stack(next_active_pane_id);
                }
                for (client_id, active_pane_id) in active_panes {
                    if active_pane_id == pane_id {
                        self.active_panes
                            .insert(client_id, next_active_pane_id, &mut self.panes);
                    }
                }
            },
            None => self.active_panes.clear(&mut self.panes),
        }
    }
    pub fn extract_pane(&mut self, pane_id: PaneId) -> Option<Box<dyn Pane>> {
        self.reset_boundaries();
        self.panes.remove(&pane_id)
    }
    pub fn remove_pane(&mut self, pane_id: PaneId) -> Option<Box<dyn Pane>> {
        let mut pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        if pane_grid.fill_space_over_pane(pane_id) {
            // successfully filled space over pane
            let closed_pane = self.panes.remove(&pane_id);
            self.move_clients_out_of_pane(pane_id);
            self.set_pane_frames(self.draw_pane_frames); // recalculate pane frames and update size
            closed_pane
        } else {
            let closed_pane = self.panes.remove(&pane_id);
            if closed_pane
                .as_ref()
                .map(|p| p.selectable())
                .unwrap_or(false)
            {
                // this is a bit of a roundabout way to say: this is the last pane and so the tab
                // should be destroyed
                self.active_panes.clear(&mut self.panes);
            }
            closed_pane
        }
    }
    pub fn hold_pane(
        &mut self,
        pane_id: PaneId,
        exit_status: Option<i32>,
        is_first_run: bool,
        run_command: RunCommand,
    ) {
        self.panes
            .get_mut(&pane_id)
            .map(|p| p.hold(exit_status, is_first_run, run_command));
    }
    pub fn panes_to_hide_contains(&self, pane_id: PaneId) -> bool {
        self.panes_to_hide.contains(&pane_id)
    }
    pub fn fullscreen_is_active(&self) -> bool {
        self.fullscreen_is_active.is_some()
    }
    pub fn unset_fullscreen(&mut self) {
        if let Some(fullscreen_pane_id) = self.fullscreen_is_active {
            let panes_to_hide: Vec<_> = self.panes_to_hide.iter().copied().collect();
            for pane_id in panes_to_hide {
                let pane = self.get_pane_mut(pane_id).unwrap();
                pane.set_should_render(true);
                pane.set_should_render_boundaries(true);
            }
            let viewport_pane_ids: Vec<_> = self
                .panes
                .keys()
                .copied()
                .into_iter()
                .filter(|id| {
                    !is_inside_viewport(&*self.viewport.borrow(), self.get_pane(*id).unwrap())
                })
                .collect();
            for pid in viewport_pane_ids {
                let viewport_pane = self.get_pane_mut(pid).unwrap();
                viewport_pane.reset_size_and_position_override();
            }
            self.panes_to_hide.clear();
            if let Some(fullscreen_pane) = self.get_pane_mut(fullscreen_pane_id) {
                fullscreen_pane.reset_size_and_position_override();
            }
            self.set_force_render();
            let display_area = *self.display_area.borrow();
            self.resize(display_area);
            self.fullscreen_is_active = None;
        }
    }
    pub fn toggle_active_pane_fullscreen(&mut self, client_id: ClientId) {
        if let Some(active_pane_id) = self.get_active_pane_id(client_id) {
            self.toggle_pane_fullscreen(active_pane_id);
        }
    }

    pub fn toggle_pane_fullscreen(&mut self, pane_id: PaneId) {
        if self.fullscreen_is_active.is_some() {
            self.unset_fullscreen();
        } else {
            let pane_ids_to_hide = self.panes.iter().filter_map(|(&id, _pane)| {
                if id != pane_id
                    && is_inside_viewport(&*self.viewport.borrow(), self.get_pane(id).unwrap())
                {
                    Some(id)
                } else {
                    None
                }
            });
            self.panes_to_hide = pane_ids_to_hide.collect();
            if self.panes_to_hide.is_empty() {
                // nothing to do, pane is already as fullscreen as it can be, let's bail
                return;
            } else {
                // For all of the panes outside of the viewport staying on the fullscreen
                // screen, switch them to using override positions as well so that the resize
                // system doesn't get confused by viewport and old panes that no longer line up
                let viewport_pane_ids: Vec<_> = self
                    .panes
                    .keys()
                    .copied()
                    .into_iter()
                    .filter(|id| {
                        !is_inside_viewport(&*self.viewport.borrow(), self.get_pane(*id).unwrap())
                    })
                    .collect();
                for pid in viewport_pane_ids {
                    if let Some(viewport_pane) = self.get_pane_mut(pid) {
                        viewport_pane.set_geom_override(viewport_pane.position_and_size());
                    }
                }
                let viewport = { *self.viewport.borrow() };
                if let Some(active_pane) = self.get_pane_mut(pane_id) {
                    let full_screen_geom = PaneGeom {
                        x: viewport.x,
                        y: viewport.y,
                        ..Default::default()
                    };
                    active_pane.set_geom_override(full_screen_geom);
                }
            }
            let connected_client_list: Vec<ClientId> =
                { self.connected_clients.borrow().iter().copied().collect() };
            for client_id in connected_client_list {
                self.focus_pane(pane_id, client_id);
            }
            self.set_force_render();
            let display_area = *self.display_area.borrow();
            self.resize(display_area);
            self.fullscreen_is_active = Some(pane_id);
        }
    }

    pub fn focus_pane_left_fullscreen(&mut self, client_id: ClientId) -> bool {
        self.unset_fullscreen();
        let ret = self.move_focus_left(client_id);
        self.toggle_active_pane_fullscreen(client_id);
        return ret;
    }

    pub fn focus_pane_right_fullscreen(&mut self, client_id: ClientId) -> bool {
        self.unset_fullscreen();
        let ret = self.move_focus_right(client_id);
        self.toggle_active_pane_fullscreen(client_id);
        return ret;
    }

    pub fn focus_pane_up_fullscreen(&mut self, client_id: ClientId) {
        self.unset_fullscreen();
        self.move_focus_up(client_id);
        self.toggle_active_pane_fullscreen(client_id);
    }

    pub fn focus_pane_down_fullscreen(&mut self, client_id: ClientId) {
        self.unset_fullscreen();
        self.move_focus_down(client_id);
        self.toggle_active_pane_fullscreen(client_id);
    }

    pub fn switch_next_pane_fullscreen(&mut self, client_id: ClientId) {
        self.unset_fullscreen();
        self.focus_next_pane(client_id);
        self.toggle_active_pane_fullscreen(client_id);
    }

    pub fn switch_prev_pane_fullscreen(&mut self, client_id: ClientId) {
        self.unset_fullscreen();
        self.focus_previous_pane(client_id);
        self.toggle_active_pane_fullscreen(client_id);
    }

    pub fn panes_to_hide_count(&self) -> usize {
        self.panes_to_hide.len()
    }
    pub fn visible_panes_count(&self) -> usize {
        self.panes.len().saturating_sub(self.panes_to_hide.len())
    }
    pub fn add_to_hidden_panels(&mut self, pid: PaneId) {
        self.panes_to_hide.insert(pid);
    }
    pub fn remove_from_hidden_panels(&mut self, pid: PaneId) {
        self.panes_to_hide.remove(&pid);
    }
    pub fn unfocus_all_panes(&mut self) {
        self.active_panes.unfocus_all_panes(&mut self.panes);
    }
    pub fn focus_all_panes(&mut self) {
        self.active_panes.focus_all_panes(&mut self.panes);
    }
    pub fn drain(&mut self) -> BTreeMap<PaneId, Box<dyn Pane>> {
        match self.panes.iter().next().map(|(pid, _p)| *pid) {
            Some(first_pid) => self.panes.split_off(&first_pid),
            None => BTreeMap::new(),
        }
    }
    pub fn active_panes(&self) -> ActivePanes {
        self.active_panes.clone()
    }
    pub fn set_active_panes(&mut self, active_panes: ActivePanes) {
        self.active_panes = active_panes;
    }
    fn move_clients_between_panes(&mut self, from_pane_id: PaneId, to_pane_id: PaneId) {
        let clients_in_pane: Vec<ClientId> = self
            .active_panes
            .iter()
            .filter(|(_cid, pid)| **pid == from_pane_id)
            .map(|(cid, _pid)| *cid)
            .collect();
        for client_id in clients_in_pane {
            self.active_panes.remove(&client_id, &mut self.panes);
            self.active_panes
                .insert(client_id, to_pane_id, &mut self.panes);
        }
    }
    fn reset_boundaries(&mut self) {
        self.client_id_to_boundaries.clear();
    }
    pub fn get_plugin_pane_id(&self, run_plugin_or_alias: &RunPluginOrAlias) -> Option<PaneId> {
        self.panes
            .iter()
            .find(|(_id, pane)| run_plugin_or_alias.is_equivalent_to_run(pane.invoked_with()))
            .map(|(id, _)| *id)
    }
    pub fn pane_info(&self, current_pane_group: &HashMap<ClientId, Vec<PaneId>>) -> Vec<PaneInfo> {
        let mut pane_infos = vec![];
        for (pane_id, pane) in self.panes.iter() {
            let mut pane_info_for_pane = pane_info_for_pane(pane_id, pane, &current_pane_group);
            let is_focused = self.active_panes.pane_id_is_focused(pane_id);
            pane_info_for_pane.is_floating = false;
            pane_info_for_pane.is_suppressed = false;
            pane_info_for_pane.is_focused = is_focused;
            pane_info_for_pane.is_fullscreen = is_focused && self.fullscreen_is_active();
            pane_infos.push(pane_info_for_pane);
        }
        pane_infos
    }

    pub fn pane_id_is_focused(&self, pane_id: &PaneId) -> bool {
        self.active_panes.pane_id_is_focused(pane_id)
    }

    pub fn update_pane_themes(&mut self, theme: Styling) {
        self.style.colors = theme;
        for pane in self.panes.values_mut() {
            pane.update_theme(theme);
        }
    }
    pub fn update_pane_arrow_fonts(&mut self, should_support_arrow_fonts: bool) {
        for pane in self.panes.values_mut() {
            pane.update_arrow_fonts(should_support_arrow_fonts);
        }
    }
    pub fn update_pane_rounded_corners(&mut self, rounded_corners: bool) {
        self.style.rounded_corners = rounded_corners;
        for pane in self.panes.values_mut() {
            pane.update_rounded_corners(rounded_corners);
        }
    }
    pub fn stack_panes(
        &mut self,
        root_pane_id: PaneId,
        pane_count_in_stack: usize,
    ) -> Vec<PaneGeom> {
        StackedPanes::new_from_btreemap(&mut self.panes, &self.panes_to_hide)
            .new_stack(root_pane_id, pane_count_in_stack)
    }
    fn is_connected(&self, client_id: &ClientId) -> bool {
        self.connected_clients.borrow().contains(&client_id)
    }
    pub fn stacked_pane_ids_under_and_over_flexible_panes(
        &mut self,
    ) -> Result<(HashSet<PaneId>, HashSet<PaneId>)> {
        StackedPanes::new_from_btreemap(&mut self.panes, &self.panes_to_hide)
            .stacked_pane_ids_under_and_over_flexible_panes()
    }
    pub fn next_selectable_pane_id_above(&mut self, pane_id: &PaneId) -> Option<PaneId> {
        let pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        let include_panes_in_stack = true;
        pane_grid.next_selectable_pane_id_above(&pane_id, include_panes_in_stack)
    }
    pub fn next_selectable_pane_id_below(&mut self, pane_id: &PaneId) -> Option<PaneId> {
        let pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        let include_panes_in_stack = true;
        pane_grid.next_selectable_pane_id_below(&pane_id, include_panes_in_stack)
    }
    pub fn next_selectable_pane_id_to_the_left(&mut self, pane_id: &PaneId) -> Option<PaneId> {
        let pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        pane_grid.next_selectable_pane_id_to_the_left(&pane_id)
    }
    pub fn next_selectable_pane_id_to_the_right(&mut self, pane_id: &PaneId) -> Option<PaneId> {
        let pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        pane_grid.next_selectable_pane_id_to_the_right(&pane_id)
    }
}

#[allow(clippy::borrowed_box)]
pub fn is_inside_viewport(viewport: &Viewport, pane: &Box<dyn Pane>) -> bool {
    let pane_position_and_size = pane.current_geom();
    pane_position_and_size.y >= viewport.y
        && pane_position_and_size.y + pane_position_and_size.rows.as_usize()
            <= viewport.y + viewport.rows
}

pub fn pane_geom_is_inside_viewport(viewport: &Viewport, geom: &PaneGeom) -> bool {
    geom.y >= viewport.y
        && geom.y + geom.rows.as_usize() <= viewport.y + viewport.rows
        && geom.x >= viewport.x
        && geom.x + geom.cols.as_usize() <= viewport.x + viewport.cols
}
