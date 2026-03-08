pub mod packages;

/// Layout definition with metadata for the TUI picker.
#[derive(Debug, Clone)]
pub struct LayoutDef {
    pub name: &'static str,
    pub description: &'static str,
    pub preview: &'static str,
}

/// All built-in layouts with descriptions and ASCII previews.
pub static LAYOUTS: &[LayoutDef] = &[
    LayoutDef {
        name: "default",
        description: "Editor + terminal + files, with DevBox management tab",
        preview: r#"
[Workspace] [DevBox] [Shell] [Git]     <- tab bar (top)
+------------------+------------------+
|  nvim .    60%   |                  |
|  (editor)        |  yazi            |
+------------------+  (files)   50%   |
|  terminal  40%   |  bat/glow        |
+------------------+------------------+
       50%                50%
<Ctrl+t> new pane  <Alt+n> new tab    <- shortcuts (bottom)

Tab 2 [DevBox]:
+------------------+------------------+
|  nvim .    60%   |  help/guide 50%  |
|  (editor)        +------------------+
+------------------+  packages TUI    |
|  terminal  40%   |           50%    |
+------------------+------------------+
"#,
    },
    LayoutDef {
        name: "ai-pair",
        description: "AI assistant + editor + output",
        preview: r#"
+------------+----------------+------------+
|            |                |            |
|  claude    |  nvim .        |  output    |
|  (30%)     |  (40%)         |  (30%)     |
|            |                |            |
+------------+----------------+------------+
Tabs: [ai-pair] [aider] [git]
"#,
    },
    LayoutDef {
        name: "fullstack",
        description: "Frontend + backend + containers + logs",
        preview: r#"
+------------------+------------------+
|  backend         |  frontend        |
+------------------+------------------+
|  lazydocker      |  api test        |
+------------------+------------------+
|  logs                               |
+-------------------------------------+
Tabs: [dev] [editor] [git]
"#,
    },
    LayoutDef {
        name: "tdd",
        description: "Editor + auto-running tests",
        preview: r#"
+------------------+------------------+
|                  |                  |
|  nvim .          |  tests           |
|  (50%)           |  (50%)           |
|                  |                  |
+------------------+------------------+
Tabs: [tdd] [git]
"#,
    },
    LayoutDef {
        name: "debug",
        description: "Source + debugger + logs + monitor",
        preview: r#"
+------------------+------------------+
|  nvim (source)   |  debugger        |
+------------------+------------------+
|  logs            |  btm (monitor)   |
+------------------+------------------+
Tabs: [debug] [shell]
"#,
    },
    LayoutDef {
        name: "monitor",
        description: "System monitoring dashboard",
        preview: r#"
+------------------+------------------+
|  htop            |  lazydocker      |
+------------------+------------------+
|  btm             |  bandwhich       |
+------------------+------------------+
"#,
    },
    LayoutDef {
        name: "git-review",
        description: "Code review: lazygit + diff + PR",
        preview: r#"
+------------------+------------------+
|  lazygit         |  diff viewer     |
+------------------+------------------+
|  PR comments                        |
+-------------------------------------+
"#,
    },
    LayoutDef {
        name: "presentation",
        description: "Minimal clean mode for demos",
        preview: r#"
+-------------------------------------+
|                                     |
|              $ _                    |
|         (single clean pane)         |
|                                     |
+-------------------------------------+
"#,
    },
    LayoutDef {
        name: "plain",
        description: "No layout, just a shell",
        preview: r#"
+-------------------------------------+
|                                     |
|              $ _                    |
|         (no zellij)                 |
|                                     |
+-------------------------------------+
"#,
    },
];

/// Find a layout by name.
pub fn find_layout(name: &str) -> Option<&'static LayoutDef> {
    LAYOUTS.iter().find(|l| l.name == name)
}

/// Embedded layout KDL files — compiled into the binary so they can
/// be pushed into the VM at attach time.
pub static LAYOUT_FILES: &[(&str, &str)] = &[
    ("default", include_str!("../../layouts/default.kdl")),
    ("ai-pair", include_str!("../../layouts/ai-pair.kdl")),
    ("fullstack", include_str!("../../layouts/fullstack.kdl")),
    ("tdd", include_str!("../../layouts/tdd.kdl")),
    ("debug", include_str!("../../layouts/debug.kdl")),
    ("monitor", include_str!("../../layouts/monitor.kdl")),
    ("git-review", include_str!("../../layouts/git-review.kdl")),
    ("presentation", include_str!("../../layouts/presentation.kdl")),
];

/// Look up a layout KDL file by name. Falls back to default.
pub fn lookup_layout_kdl(name: &str) -> &'static str {
    LAYOUT_FILES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, c)| *c)
        .unwrap_or(LAYOUT_FILES[0].1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_layouts_present() {
        assert_eq!(LAYOUTS.len(), 9);
        assert!(find_layout("default").is_some());
        assert!(find_layout("ai-pair").is_some());
        assert!(find_layout("plain").is_some());
        assert!(find_layout("nonexistent").is_none());
    }

    #[test]
    fn layout_has_description() {
        let layout = find_layout("tdd").unwrap();
        assert!(!layout.description.is_empty());
        assert!(!layout.preview.is_empty());
    }
}
