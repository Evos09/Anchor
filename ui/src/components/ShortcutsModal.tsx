interface ShortcutGroup {
  category: string;
  items: { key: string; description: string }[];
}

const SHORTCUT_GROUPS: ShortcutGroup[] = [
  {
    category: "Global Navigation & Layout",
    items: [
      { key: "Ctrl + K", description: "Universal Command Palette (Commands, Slash Tasks, Presets)" },
      { key: "Ctrl + P", description: "Quick Open File in Code Workbench" },
      { key: "Ctrl + J", description: "Toggle Antigravity Agent Sidecar Dock" },
      { key: "Ctrl + 1 .. 8", description: "Switch Activity Rail View (Mission, Code, Plans, Terminal, etc.)" },
      { key: "? or Ctrl + /", description: "Toggle this Keyboard Shortcuts Cheat Sheet" },
      { key: "Esc", description: "Close Command Palette or Active Modal" },
    ],
  },
  {
    category: "Code Workbench & Editor",
    items: [
      { key: "Ctrl + S", description: "Save & Commit Buffer with fast SHA-256 snapshot" },
      { key: "Ctrl + F", description: "Focus Find-in-Buffer search filter" },
      { key: "Line Click", description: "Select line range to dispatch reference to Agent Sidecar" },
      { key: "+ New", description: "Create a new file directly in workspace" },
      { key: "Delete", description: "Safe workspace file deletion with snapshot tracking" },
    ],
  },
  {
    category: "Sandboxed Terminal & Tasks",
    items: [
      { key: "Enter", description: "Execute command confined inside workspace sandbox" },
      { key: "↑ / ↓", description: "Navigate previous command history in terminal input" },
      { key: "Presets", description: "1-Click test, lint, and git status execution" },
      { key: "Ask Agent", description: "Send stdout / stderr failure trace to Agent Sidecar" },
    ],
  },
  {
    category: "Antigravity Agent Sidecar",
    items: [
      { key: "Enter", description: "Dispatch prompt / instruction to Cordis Agent" },
      { key: "Shift + Enter", description: "Insert newline without dispatching" },
      { key: "+ @workspace", description: "Inject entire repository context" },
      { key: "+ @file", description: "Inject active open file reference" },
      { key: "▲ Fold / ▼ Show", description: "Toggle Chain-of-Thought reasoning disclosure" },
    ],
  },
];

export function ShortcutsModal({
  open,
  onClose,
}: {
  open: boolean;
  onClose: () => void;
}) {
  if (!open) return null;

  return (
    <div className="palette-backdrop" onClick={onClose} role="dialog" aria-modal="true" aria-label="Keyboard Shortcuts">
      <div className="palette-container shortcuts-modal" onClick={(e) => e.stopPropagation()}>
        <div className="palette-search-bar" style={{ justifyContent: "space-between" }}>
          <div className="row">
            <span className="palette-icon mono">⌨</span>
            <strong className="mono">ANTIGRAVITY KEYBOARD SHORTCUTS</strong>
          </div>
          <button type="button" className="btn btn-small" onClick={onClose}>
            ✕
          </button>
        </div>

        <div className="shortcuts-content">
          {SHORTCUT_GROUPS.map((group) => (
            <div key={group.category} className="shortcuts-group">
              <div className="wb-section-head">
                <span>{group.category}</span>
              </div>
              <table className="shortcuts-table">
                <tbody>
                  {group.items.map((item) => (
                    <tr key={item.key}>
                      <td className="shortcuts-key-cell">
                        <kbd className="palette-kbd">{item.key}</kbd>
                      </td>
                      <td className="shortcuts-desc-cell">{item.description}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          ))}
        </div>

        <div className="palette-footer" style={{ justifyContent: "space-between" }}>
          <span className="small muted mono">Swiss-Industrial Design System · Antigravity 2.0</span>
          <button type="button" className="btn btn-primary btn-small" onClick={onClose}>
            Got it (Esc)
          </button>
        </div>
      </div>
    </div>
  );
}
