pub mod layout_picker;
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
        description: "Clean workspace: editor + terminal + files",
        preview: r#"
+------------------+------------------+
|                  |                  |
|  nvim .          |  terminal        |
|  (editor - 60%) |  (60%)           |
|                  +------------------+
|                  |  yazi            |
|                  |  (files - 40%)   |
+------------------+------------------+
Tabs: [workspace] [shell] [git]
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
