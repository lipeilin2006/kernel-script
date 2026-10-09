//! The `draw` module: overlay primitives queued per frame for the render
//! thread.

use std::sync::Arc;

use mlua::Lua;

use super::{DrawCommand, DrawCommands};

pub fn register_draw_api(lua: &Lua, draw_commands: DrawCommands) -> mlua::Result<()> {
    let module = lua.create_table()?;

    {
        let dc = Arc::clone(&draw_commands);
        module.set(
            "line",
            lua.create_function(
                move |_,
                      (x1, y1, x2, y2, r, g, b, a, thickness): (
                    f32,
                    f32,
                    f32,
                    f32,
                    u8,
                    u8,
                    u8,
                    u8,
                    f32,
                )| {
                    dc.lock()
                        .map_err(|_| mlua::Error::runtime("draw lock poisoned"))?
                        .push(DrawCommand::Line {
                            x1,
                            y1,
                            x2,
                            y2,
                            color: [r, g, b, a],
                            thickness,
                        });
                    Ok(())
                },
            )?,
        )?;
    }

    {
        let dc = Arc::clone(&draw_commands);
        module.set(
            "rect",
            lua.create_function(
                move |_,
                      (x, y, w, h, r, g, b, a, thickness): (
                    f32,
                    f32,
                    f32,
                    f32,
                    u8,
                    u8,
                    u8,
                    u8,
                    f32,
                )| {
                    dc.lock()
                        .map_err(|_| mlua::Error::runtime("draw lock poisoned"))?
                        .push(DrawCommand::Rect {
                            x,
                            y,
                            w,
                            h,
                            color: [r, g, b, a],
                            thickness,
                        });
                    Ok(())
                },
            )?,
        )?;
    }

    {
        let dc = Arc::clone(&draw_commands);
        module.set(
            "filled_rect",
            lua.create_function(
                move |_, (x, y, w, h, r, g, b, a): (f32, f32, f32, f32, u8, u8, u8, u8)| {
                    dc.lock()
                        .map_err(|_| mlua::Error::runtime("draw lock poisoned"))?
                        .push(DrawCommand::FilledRect {
                            x,
                            y,
                            w,
                            h,
                            color: [r, g, b, a],
                        });
                    Ok(())
                },
            )?,
        )?;
    }

    {
        let dc = Arc::clone(&draw_commands);
        module.set(
            "circle",
            lua.create_function(
                move |_,
                      (x, y, radius, r, g, b, a, thickness): (
                    f32,
                    f32,
                    f32,
                    u8,
                    u8,
                    u8,
                    u8,
                    f32,
                )| {
                    dc.lock()
                        .map_err(|_| mlua::Error::runtime("draw lock poisoned"))?
                        .push(DrawCommand::Circle {
                            x,
                            y,
                            radius,
                            color: [r, g, b, a],
                            thickness,
                        });
                    Ok(())
                },
            )?,
        )?;
    }

    {
        let dc = Arc::clone(&draw_commands);
        module.set(
            "filled_circle",
            lua.create_function(
                move |_, (x, y, radius, r, g, b, a): (f32, f32, f32, u8, u8, u8, u8)| {
                    dc.lock()
                        .map_err(|_| mlua::Error::runtime("draw lock poisoned"))?
                        .push(DrawCommand::FilledCircle {
                            x,
                            y,
                            radius,
                            color: [r, g, b, a],
                        });
                    Ok(())
                },
            )?,
        )?;
    }

    {
        let dc = Arc::clone(&draw_commands);
        module.set(
            "text",
            lua.create_function(
                move |_, (x, y, text, r, g, b, a, size): (f32, f32, String, u8, u8, u8, u8, f32)| {
                    dc.lock()
                        .map_err(|_| mlua::Error::runtime("draw lock poisoned"))?
                        .push(DrawCommand::Text {
                            x,
                            y,
                            text,
                            color: [r, g, b, a],
                            size,
                        });
                    Ok(())
                },
            )?,
        )?;
    }

    lua.globals().set("draw", module)
}
