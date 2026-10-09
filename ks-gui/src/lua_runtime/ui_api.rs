//! The per-frame `ui` module: egui widgets reachable from Lua `OnUpdate`.
//!
//! [`create_ui_module`] builds one table per frame inside a Lua scope; the
//! bridge pointer hands the current `egui::Ui` to the widget closures for
//! the duration of one call.

use std::sync::{Arc, Mutex};

use mlua::{Function, Lua, Table};

pub fn create_ui_module<'scope>(
    lua: &Lua,
    scope: &'scope mlua::Scope<'scope, '_>,
    ctx: &'scope egui::Context,
    ui_visible: bool,
) -> mlua::Result<Table> {
    let module = lua.create_table()?;
    let bridge = Arc::new(Mutex::new(0usize));
    add_window(&module, scope, ctx, ui_visible, &bridge)?;
    add_basic_widgets(&module, scope, &bridge)?;
    add_sliders(&module, scope, &bridge)?;
    add_choices(&module, scope, &bridge)?;
    add_containers(&module, scope, &bridge)?;
    add_text_widgets(&module, scope, &bridge)?;
    Ok(module)
}

/// `ui.window(title, body)` — the root container every other widget draws
/// into.
fn add_window<'scope>(
    module: &Table,
    scope: &'scope mlua::Scope<'scope, '_>,
    ctx: &'scope egui::Context,
    ui_visible: bool,
    bridge: &Arc<Mutex<usize>>,
) -> mlua::Result<()> {
    let bridge = Arc::clone(bridge);
    let bridge_for_window = Arc::clone(&bridge);
    module.set(
        "window",
        scope.create_function(move |_, (title, body): (String, Function)| {
            // While the script UI is hidden the window body is skipped entirely:
            // OnUpdate keeps running so draw.* overlay output is unaffected, and
            // scripts never observe a missing ui.window function.
            if !ui_visible {
                return Ok(());
            }
            let callback_error = std::cell::RefCell::new(None);
            egui::Window::new(title).show(ctx, |ui| {
                *bridge_for_window.lock().unwrap() = ui as *mut egui::Ui as usize;
                if let Err(error) = body.call::<()>(()) {
                    *callback_error.borrow_mut() = Some(error);
                }
                *bridge_for_window.lock().unwrap() = 0;
            });
            match callback_error.into_inner() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })?,
    )?;
    Ok(())
}

/// Plain widgets: text, buttons, toggles, numeric drags, the text editor
/// and the color picker.
fn add_basic_widgets<'scope>(
    module: &Table,
    scope: &'scope mlua::Scope<'scope, '_>,
    bridge: &Arc<Mutex<usize>>,
) -> mlua::Result<()> {
    let bridge = Arc::clone(bridge);
    module.set("label", {
        let bridge = Arc::clone(&bridge);
        scope.create_function(move |_, text: String| {
            with_egui_ui(&bridge, |ui| ui.label(text))?;
            Ok(())
        })?
    })?;
    module.set("button", {
        let bridge = Arc::clone(&bridge);
        scope.create_function(move |_, label: String| {
            with_egui_ui(&bridge, |ui| ui.button(label).clicked())
        })?
    })?;
    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "separator",
            scope.create_function(move |_, ()| {
                with_egui_ui(&bridge, |ui| ui.separator())?;
                Ok(())
            })?,
        )?;
    }
    module.set("checkbox", {
        let bridge = Arc::clone(&bridge);
        scope.create_function(move |_, (label, mut value): (String, bool)| {
            with_egui_ui(&bridge, |ui| {
                let changed = ui.checkbox(&mut value, label).changed();
                (value, changed)
            })
        })?
    })?;
    module.set("drag_value", {
        let bridge = Arc::clone(&bridge);
        scope.create_function(move |_, (label, mut value): (String, f64)| {
            with_egui_ui(&bridge, |ui| {
                let changed = ui
                    .add(egui::DragValue::new(&mut value).prefix(format!("{}: ", label)))
                    .changed();
                (value, changed)
            })
        })?
    })?;
    macro_rules! drag_value {
        ($name:literal, $ty:ty) => {{
            let bridge = Arc::clone(&bridge);
            module.set(
                $name,
                scope.create_function(move |_, (label, mut value): (String, $ty)| {
                    with_egui_ui(&bridge, |ui| {
                        let changed = ui
                            .add(egui::DragValue::new(&mut value).prefix(format!("{}: ", label)))
                            .changed();
                        (value, changed)
                    })
                })?,
            )?;
        }};
    }
    drag_value!("drag_value_i8", i8);
    drag_value!("drag_value_u8", u8);
    drag_value!("drag_value_i16", i16);
    drag_value!("drag_value_u16", u16);
    drag_value!("drag_value_i32", i32);
    drag_value!("drag_value_u32", u32);
    drag_value!("drag_value_i64", i64);
    drag_value!("drag_value_u64", u64);
    drag_value!("drag_value_f32", f32);
    module.set("text_edit", {
        let bridge = Arc::clone(&bridge);
        scope.create_function(
            move |_, (mut value, multiline, password, code): (String, bool, bool, bool)| {
                with_egui_ui(&bridge, |ui| {
                    let mut editor = if multiline {
                        egui::TextEdit::multiline(&mut value).desired_rows(8)
                    } else {
                        egui::TextEdit::singleline(&mut value)
                    };
                    editor = editor.password(password);
                    if code {
                        editor = editor.code_editor();
                    }
                    let changed = ui.add(editor).changed();
                    (value, changed)
                })
            },
        )?
    })?;
    module.set("color_edit_button_srgba", {
        let bridge = Arc::clone(&bridge);
        scope.create_function(move |_, (r, g, b, a): (u8, u8, u8, u8)| {
            with_egui_ui(&bridge, |ui| {
                let mut color = egui::Color32::from_rgba_unmultiplied(r, g, b, a);
                let changed = ui.color_edit_button_srgba(&mut color).changed();
                let [r, g, b, a] = color.to_array();
                (r, g, b, a, changed)
            })
        })?
    })?;
    Ok(())
}

/// One slider per numeric type.
fn add_sliders<'scope>(
    module: &Table,
    scope: &'scope mlua::Scope<'scope, '_>,
    bridge: &Arc<Mutex<usize>>,
) -> mlua::Result<()> {
    let bridge = Arc::clone(bridge);
    macro_rules! slider {
        ($name:literal, $ty:ty) => {{
            let bridge = Arc::clone(&bridge);
            module.set(
                $name,
                scope.create_function(
                    move |_, (label, mut value, min, max): (String, $ty, $ty, $ty)| {
                        with_egui_ui(&bridge, |ui| {
                            let changed = ui
                                .add(egui::Slider::new(&mut value, min..=max).text(&label))
                                .changed();
                            (value, changed)
                        })
                    },
                )?,
            )?;
        }};
    }
    slider!("slider_i8", i8);
    slider!("slider_u8", u8);
    slider!("slider_i16", i16);
    slider!("slider_u16", u16);
    slider!("slider_i32", i32);
    slider!("slider_u32", u32);
    slider!("slider_i64", i64);
    slider!("slider_u64", u64);
    slider!("slider_f32", f32);
    slider!("slider_f64", f64);
    Ok(())
}

/// Single-choice widgets: combo box (dropdown), radio buttons and the
/// progress bar.
fn add_choices<'scope>(
    module: &Table,
    scope: &'scope mlua::Scope<'scope, '_>,
    bridge: &Arc<Mutex<usize>>,
) -> mlua::Result<()> {
    let bridge = Arc::clone(bridge);

    // Combo box (dropdown)
    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "combo_box",
            scope.create_function(
                move |_, (label, mut selected, options): (String, String, Vec<String>)| {
                    with_egui_ui(&bridge, |ui| {
                        let mut changed = false;
                        egui::ComboBox::from_label(&label)
                            .selected_text(&selected)
                            .show_ui(ui, |ui| {
                                for option in &options {
                                    let is_selected = *option == selected;
                                    if ui.selectable_label(is_selected, option).clicked()
                                        && !is_selected
                                    {
                                        selected = option.clone();
                                        changed = true;
                                    }
                                }
                            });
                        (selected, changed)
                    })
                },
            )?,
        )?;
    }

    // Simpler radio: pass label + current selected, return whether this label is now selected
    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "radio",
            scope.create_function(move |_, (label, group_selected): (String, String)| {
                with_egui_ui(&bridge, |ui| {
                    let response = ui.radio(label == group_selected, &label);
                    (
                        label == group_selected,
                        response.clicked() && label != group_selected,
                    )
                })
            })?,
        )?;
    }

    // Progress bar
    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "progress_bar",
            scope.create_function(move |_, (fraction, label): (f32, String)| {
                with_egui_ui(&bridge, |ui| {
                    ui.add(
                        egui::ProgressBar::new(fraction.clamp(0.0, 1.0))
                            .show_percentage()
                            .text(&label),
                    );
                })?;
                Ok(())
            })?,
        )?;
    }

    Ok(())
}

/// Nested containers: their bodies run against a swapped bridge target so
/// Lua callbacks draw into the child Ui.
fn add_containers<'scope>(
    module: &Table,
    scope: &'scope mlua::Scope<'scope, '_>,
    bridge: &Arc<Mutex<usize>>,
) -> mlua::Result<()> {
    let bridge = Arc::clone(bridge);

    // Collapsing header
    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "collapsing_header",
            scope.create_function(move |_, (title, body): (String, Function)| {
                let callback_error = std::cell::RefCell::new(None);
                with_egui_ui(&bridge, |ui| {
                    egui::CollapsingHeader::new(&title)
                        .default_open(false)
                        .show(ui, |inner_ui| {
                            if let Err(error) =
                                with_egui_ui_swapped(&bridge, inner_ui, |_ui| body.call::<()>(()))
                            {
                                *callback_error.borrow_mut() = Some(error);
                            }
                        });
                })?;
                match callback_error.into_inner() {
                    Some(error) => Err(error),
                    None => Ok(()),
                }
            })?,
        )?;
    }

    // Scroll area
    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "scroll_area",
            scope.create_function(move |_, (height, body): (f32, Function)| {
                let callback_error = std::cell::RefCell::new(None);
                with_egui_ui(&bridge, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(height.max(32.0))
                        .show(ui, |inner_ui| {
                            if let Err(error) =
                                with_egui_ui_swapped(&bridge, inner_ui, |_ui| body.call::<()>(()))
                            {
                                *callback_error.borrow_mut() = Some(error);
                            }
                        });
                })?;
                match callback_error.into_inner() {
                    Some(error) => Err(error),
                    None => Ok(()),
                }
            })?,
        )?;
    }

    Ok(())
}

/// Labels and chrome: list items, headings, monospace/code text, links and
/// spacing.
fn add_text_widgets<'scope>(
    module: &Table,
    scope: &'scope mlua::Scope<'scope, '_>,
    bridge: &Arc<Mutex<usize>>,
) -> mlua::Result<()> {
    let bridge = Arc::clone(bridge);

    // Selectable label (list item)
    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "selectable_label",
            scope.create_function(move |_, (label, selected): (String, bool)| {
                with_egui_ui(&bridge, |ui| {
                    let response = ui.selectable_label(selected, &label);
                    (response.clicked(), selected)
                })
            })?,
        )?;
    }

    // Heading text
    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "heading",
            scope.create_function(move |_, text: String| {
                with_egui_ui(&bridge, |ui| {
                    ui.heading(&text);
                })?;
                Ok(())
            })?,
        )?;
    }

    // Monospace text
    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "monospace",
            scope.create_function(move |_, text: String| {
                with_egui_ui(&bridge, |ui| {
                    ui.monospace(&text);
                })?;
                Ok(())
            })?,
        )?;
    }

    // Hyperlink
    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "hyperlink_to",
            scope.create_function(move |_, (label, url): (String, String)| {
                with_egui_ui(&bridge, |ui| {
                    ui.hyperlink_to(&label, &url);
                })?;
                Ok(())
            })?,
        )?;
    }

    for (name, kind) in [("small", 0u8), ("weak", 1u8), ("code", 2u8)] {
        let bridge = Arc::clone(&bridge);
        module.set(
            name,
            scope.create_function(move |_, text: String| {
                with_egui_ui(&bridge, |ui| match kind {
                    0 => ui.small(text),
                    1 => ui.weak(text),
                    _ => ui.code(text),
                })?;
                Ok(())
            })?,
        )?;
    }

    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "add_space",
            scope.create_function(move |_, amount: f32| {
                with_egui_ui(&bridge, |ui| ui.add_space(amount.max(0.0)))?;
                Ok(())
            })?,
        )?;
    }

    // Spinner
    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "spinner",
            scope.create_function(move |_, ()| {
                with_egui_ui(&bridge, |ui| {
                    ui.spinner();
                })?;
                Ok(())
            })?,
        )?;
    }

    Ok(())
}

fn with_egui_ui<R>(
    bridge: &Mutex<usize>,
    draw: impl FnOnce(&mut egui::Ui) -> R,
) -> Result<R, mlua::Error> {
    let pointer = *bridge
        .lock()
        .map_err(|_| mlua::Error::runtime("egui bridge lock poisoned"))?
        as *mut egui::Ui;
    if pointer.is_null() {
        return Err(mlua::Error::runtime(
            "UI operation called outside ui.window",
        ));
    }
    // The bridge is created and consumed synchronously on the GUI thread. Lua
    // cannot yield from an egui callback, so the pointer is valid for this call.
    Ok(unsafe { draw(&mut *pointer) })
}

/// Runs `draw` against a nested Ui (the child Ui handed out by container
/// widgets such as CollapsingHeader or ScrollArea) for the duration of the
/// call, then restores the previous bridge target. Without the swap, Lua body
/// callbacks would draw into the parent Ui and overlap surrounding content.
fn with_egui_ui_swapped<R>(
    bridge: &Mutex<usize>,
    nested: &mut egui::Ui,
    draw: impl FnOnce(&mut egui::Ui) -> R,
) -> Result<R, mlua::Error> {
    let previous = {
        let mut guard = bridge
            .lock()
            .map_err(|_| mlua::Error::runtime("egui bridge lock poisoned"))?;
        let previous = *guard;
        *guard = nested as *mut egui::Ui as usize;
        previous
    };
    let result = with_egui_ui(bridge, draw);
    if let Ok(mut guard) = bridge.lock() {
        *guard = previous;
    }
    result
}
/*
                    if let Err(error) = body.call::<()>(()) {
                        *callback_error.borrow_mut() = Some(error);
                    }
                });
            match callback_error.into_inner() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })?,
    )?;
    module.set(
        "text",
        scope.create_function(|_, text: String| {
            ui.text(text);
            Ok(())
        })?,
    )?;
    module.set(
        "text_wrapped",
        scope.create_function(|_, text: String| {
            ui.text_wrapped(text);
            Ok(())
        })?,
    )?;
    module.set(
        "button",
        scope.create_function(|_, label: String| Ok(ui.button(label)))?,
    )?;
    module.set(
        "separator",
        scope.create_function(|_, ()| {
            ui.separator();
            Ok(())
        })?,
    )?;
    module.set(
        "same_line",
        scope.create_function(|_, ()| {
            ui.same_line();
            Ok(())
        })?,
    )?;
    module.set(
        "checkbox",
        scope.create_function(|_, (label, mut value): (String, bool)| {
            let changed = ui.checkbox(label, &mut value);
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_int",
        scope.create_function(|_, (label, mut value): (String, i32)| {
            let changed = ui.input_int(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_i64",
        scope.create_function(|_, (label, mut value): (String, i64)| {
            let changed = ui.input_scalar(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_u64",
        scope.create_function(|_, (label, mut value): (String, u64)| {
            let changed = ui.input_scalar(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_i8",
        scope.create_function(|_, (label, mut value): (String, i8)| {
            let changed = ui.input_scalar(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_u8",
        scope.create_function(|_, (label, mut value): (String, u8)| {
            let changed = ui.input_scalar(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_i16",
        scope.create_function(|_, (label, mut value): (String, i16)| {
            let changed = ui.input_scalar(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_u16",
        scope.create_function(|_, (label, mut value): (String, u16)| {
            let changed = ui.input_scalar(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_u32",
        scope.create_function(|_, (label, mut value): (String, u32)| {
            let changed = ui.input_scalar(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_f32",
        scope.create_function(|_, (label, mut value): (String, f32)| {
            let changed = ui.input_scalar(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_f64",
        scope.create_function(|_, (label, mut value): (String, f64)| {
            let changed = ui.input_scalar(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_isize",
        scope.create_function(|_, (label, mut value): (String, isize)| {
            let changed = ui.input_scalar(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_usize",
        scope.create_function(|_, (label, mut value): (String, usize)| {
            let changed = ui.input_scalar(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_text",
        scope.create_function(|_, (label, mut value): (String, String)| {
            let changed = ui.input_text(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    Ok(module)
}

*/
