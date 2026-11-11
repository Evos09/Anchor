import { useCallback, useEffect, useState } from "react";
import { get, post, q } from "../api";
import type { ClaimsView, WorkspaceFileView } from "../types";
import { AgentChip, Badge, Empty, ErrorLine } from "../util";
import { HighlightedLine } from "../components/SyntaxHighlighter";

export function CodeWorkbench({
  version,
  colors,
  selectedFile,
  onSelectFile,
  onAskAgent,
  compact = false,
}: {
  version: number;
  colors: (n: string | null | undefined) => string;
  selectedFile: string | null;
  onSelectFile: (path: string) => void;
  onAskAgent?: (prompt: string) => void;
  compact?: boolean;
}) {
  const [files, setFiles] = useState<string[]>([]);
  const [openTabs, setOpenTabs] = useState<string[]>(["calc.py"]);
  const [activeTab, setActiveTab] = useState<string>("calc.py");
  const [filter, setFilter] = useState("");
  const [fileView, setFileView] = useState<WorkspaceFileView | null>(null);
  const [draft, setDraft] = useState("");
  const [editing, setEditing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [searchTerm, setSearchTerm] = useState("");
  const [selectedRange, setSelectedRange] = useState<[number, number] | null>(null);
  const [statusMsg, setStatusMsg] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  // New file creation state
  const [creatingFile, setCreatingFile] = useState(false);
  const [newFileName, setNewFileName] = useState("");

  const refreshFiles = useCallback(async () => {
    try {
      const v = await get<ClaimsView>("/api/claims");
      setFiles(v.files);
      if (v.files.length > 0 && !openTabs.length) {
        setOpenTabs([v.files[0]!]);
        setActiveTab(v.files[0]!);
      }
    } catch (e) {
      setError(String(e));
    }
  }, [openTabs.length]);

  useEffect(() => {
    void refreshFiles();
  }, [version, refreshFiles]);

  useEffect(() => {
    if (selectedFile) {
      if (!openTabs.includes(selectedFile)) {
        setOpenTabs((prev) => [...prev, selectedFile]);
      }
      setActiveTab(selectedFile);
    }
  }, [selectedFile, openTabs]);

  const loadFile = useCallback(async (path: string) => {
    try {
      const v = await get<WorkspaceFileView>(`/api/workspace-file${q({ path })}`);
      setFileView(v);
      setDraft(v.content);
      setEditing(false);
      setSelectedRange(null);
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    if (activeTab) {
      void loadFile(activeTab);
    }
  }, [activeTab, version, loadFile]);

  const switchTab = (tab: string) => {
    setActiveTab(tab);
    onSelectFile(tab);
  };

  const closeTab = (e: React.MouseEvent, tab: string) => {
    e.stopPropagation();
    const remaining = openTabs.filter((t) => t !== tab);
    setOpenTabs(remaining);
    if (activeTab === tab && remaining.length > 0) {
      const next = remaining[remaining.length - 1]!;
      setActiveTab(next);
      onSelectFile(next);
    }
  };

  const openFileFromList = (f: string) => {
    if (!openTabs.includes(f)) {
      setOpenTabs((prev) => [...prev, f]);
    }
    setActiveTab(f);
    onSelectFile(f);
  };

  const saveEdit = async () => {
    if (!fileView) return;
    setSaving(true);
    setStatusMsg(null);
    try {
      await post("/api/file/edit", { path: fileView.path, content: draft });
      setStatusMsg(`Saved ${fileView.path} (audited mutation)`);
      await loadFile(fileView.path);
      await refreshFiles();
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const handleCreateFile = async (e: React.FormEvent) => {
    e.preventDefault();
    const cleanPath = newFileName.trim().replace(/^[/\\]+/, "");
    if (!cleanPath) return;
    try {
      await post("/api/file/edit", { path: cleanPath, content: "" });
      setStatusMsg(`Created file '${cleanPath}'`);
      setNewFileName("");
      setCreatingFile(false);
      await refreshFiles();
      openFileFromList(cleanPath);
    } catch (err) {
      setError(String(err));
    }
  };

  const handleDeleteFile = async () => {
    if (!fileView) return;
    const confirmed = window.confirm(`Are you sure you want to delete '${fileView.path}'? A snapshot will be recorded.`);
    if (!confirmed) return;
    try {
      await post("/api/file/delete", { path: fileView.path });
      setStatusMsg(`Deleted ${fileView.path}`);
      const remaining = openTabs.filter((t) => t !== fileView.path);
      setOpenTabs(remaining);
      if (remaining.length > 0) {
        setActiveTab(remaining[remaining.length - 1]!);
        onSelectFile(remaining[remaining.length - 1]!);
      } else {
        setFileView(null);
      }
      await refreshFiles();
    } catch (err) {
      setError(String(err));
    }
  };

  // Keyboard shortcut Ctrl+S / Cmd+S to save
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "s") {
        if (editing) {
          e.preventDefault();
          void saveEdit();
        }
      }
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [editing, draft, fileView]);

  const handleLineClick = (lineNum: number) => {
    if (!selectedRange) {
      setSelectedRange([lineNum, lineNum]);
    } else if (selectedRange[0] === lineNum && selectedRange[1] === lineNum) {
      setSelectedRange(null);
    } else {
      const start = Math.min(selectedRange[0], lineNum);
      const end = Math.max(selectedRange[0], lineNum);
      setSelectedRange([start, end]);
    }
  };

  const askAboutSelection = () => {
    if (!fileView) return;
    const ref = selectedRange
      ? `@${fileView.path}#L${selectedRange[0]}-L${selectedRange[1]}`
      : `@${fileView.path}`;
    const prompt = selectedRange
      ? `Inspect ${ref} and review its implementation, boundary logic, and test coverage.`
      : `Analyze ${ref} for correctness and verification coverage.`;
    onAskAgent?.(prompt);
  };

  const filteredFiles = files.filter((f) => !filter || f.toLowerCase().includes(filter.toLowerCase()));
  const lines = (editing ? draft : (fileView?.content ?? "")).split("\n");
  const isDirty = editing && draft !== (fileView?.content ?? "");

  const matchingLineIndices = searchTerm
    ? lines
        .map((l, i) => (l.toLowerCase().includes(searchTerm.toLowerCase()) ? i : -1))
        .filter((i) => i >= 0)
    : [];

  return (
    <div className={compact ? "wb-compact" : "wb-split"}>
      {!compact && (
        <div className="wb-sidebar">
          <div className="wb-section-head">
            <span>WORKSPACE FILES</span>
            <div className="row">
              <button
                type="button"
                className="link small"
                onClick={() => setCreatingFile((v) => !v)}
                title="Create a new file in workspace"
              >
                + New
              </button>
              <span className="mono muted">{files.length}</span>
            </div>
          </div>

          {creatingFile && (
            <form className="wb-newfile-form" onSubmit={handleCreateFile}>
              <input
                type="text"
                className="wb-filter mono"
                placeholder="filename (e.g. app.py)…"
                autoFocus
                value={newFileName}
                onChange={(e) => setNewFileName(e.target.value)}
              />
              <div className="actions" style={{ marginTop: "0.2rem" }}>
                <button type="submit" className="btn btn-small btn-primary" disabled={!newFileName.trim()}>
                  Create
                </button>
                <button type="button" className="btn btn-small" onClick={() => setCreatingFile(false)}>
                  Cancel
                </button>
              </div>
            </form>
          )}

          <input
            type="search"
            className="wb-filter"
            placeholder="Filter files (e.g. calc.py)…"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            aria-label="Filter workspace files"
          />
          {!filteredFiles.length && <Empty>No matching files in workspace.</Empty>}
          <ul className="wb-filelist">
            {filteredFiles.map((f) => (
              <li key={f}>
                <button
                  type="button"
                  className={f === activeTab ? "wb-filebtn wb-filebtn-on" : "wb-filebtn"}
                  onClick={() => openFileFromList(f)}
                >
                  <code>{f}</code>
                </button>
              </li>
            ))}
          </ul>
        </div>
      )}

      <div className="wb-editor-pane">
        {/* MULTI-TAB EDITOR STRIP */}
        <div className="editor-tab-strip" role="tablist" aria-label="Open editor tabs">
          {openTabs.map((tab) => (
            <div
              key={tab}
              role="tab"
              aria-selected={tab === activeTab}
              className={`editor-tab ${tab === activeTab ? "editor-tab-active" : ""}`}
              onClick={() => switchTab(tab)}
            >
              <span className="mono small">{tab}</span>
              {tab === activeTab && isDirty && <span className="tab-dirty" title="Unsaved changes">●</span>}
              <button
                type="button"
                className="tab-close-btn"
                onClick={(e) => closeTab(e, tab)}
                title="Close tab"
              >
                ×
              </button>
            </div>
          ))}
          {openTabs.length === 0 && (
            <span className="small muted" style={{ padding: "0.25rem 0.5rem" }}>
              No open tabs (select a file on the left)
            </span>
          )}
        </div>

        <ErrorLine error={error} />
        {statusMsg && (
          <div className="banner banner-ok small" role="status">
            {statusMsg}
          </div>
        )}

        {!fileView ? (
          <Empty>Select a workspace file to inspect its contents, guard status, and active claims.</Empty>
        ) : (
          <>
            {/* WORKBENCH TOOLBAR */}
            <div className="wb-toolbar">
              <div className="row">
                <code className="wb-filepath">{fileView.path}</code>
                <Badge
                  kind={
                    fileView.append_only === "append_only"
                      ? "warn"
                      : fileView.append_only === "review_needed"
                        ? "info"
                        : "muted"
                  }
                >
                  guard: {fileView.append_only}
                </Badge>
                {fileView.claim && (
                  <span className="row small">
                    <AgentChip name={fileView.claim.agent} color={colors(fileView.claim.agent)} />
                    <span className="muted">({fileView.claim.claim_type} lock)</span>
                  </span>
                )}
              </div>

              {/* FIND IN BUFFER SEARCH */}
              <div className="row">
                <input
                  type="search"
                  className="wb-inpage-search mono"
                  placeholder="Find in buffer (Ctrl+F)…"
                  value={searchTerm}
                  onChange={(e) => setSearchTerm(e.target.value)}
                  aria-label="Find in buffer"
                />
                {searchTerm && (
                  <span className="small mono muted">
                    {matchingLineIndices.length} {matchingLineIndices.length === 1 ? "match" : "matches"}
                  </span>
                )}
              </div>

              <div className="row small mono muted">
                <span>{fileView.lines} LOC</span>
                <span>·</span>
                <span>{fileView.bytes} B</span>
                <span>·</span>
                <span title={`SHA-256: ${fileView.sha256}`}>sha256:{fileView.sha256.slice(0, 10)}</span>
                {fileView.editable && !editing && (
                  <button type="button" className="btn btn-small" onClick={() => setEditing(true)}>
                    Edit buffer (Ctrl+E)
                  </button>
                )}
                {editing && (
                  <>
                    <button
                      type="button"
                      className="btn btn-small btn-primary"
                      disabled={saving}
                      onClick={saveEdit}
                    >
                      {saving ? "Writing…" : "Commit write (Ctrl+S)"}
                    </button>
                    <button
                      type="button"
                      className="btn btn-small"
                      onClick={() => {
                        setDraft(fileView.content);
                        setEditing(false);
                      }}
                    >
                      Cancel
                    </button>
                  </>
                )}
                <button
                  type="button"
                  className="btn btn-small btn-plan"
                  onClick={askAboutSelection}
                  title="Send file or selection reference to Agent Sidecar"
                >
                  {selectedRange
                    ? `Ask Agent (L${selectedRange[0]}-${selectedRange[1]}) ▸`
                    : "Ask Agent (@file) ▸"}
                </button>
                <button
                  type="button"
                  className="btn btn-small btn-danger"
                  onClick={handleDeleteFile}
                  title="Delete this file"
                >
                  Delete
                </button>
              </div>
            </div>

            {/* SELECTION ACTION BAR */}
            {selectedRange && (
              <div className="wb-selection-banner small row">
                <span>
                  Selected lines <strong className="mono">L{selectedRange[0]}–L{selectedRange[1]}</strong>
                </span>
                <button type="button" className="btn btn-small btn-primary" onClick={askAboutSelection}>
                  Send selection to Agent Sidecar ▸
                </button>
                <button type="button" className="btn btn-small" onClick={() => setSelectedRange(null)}>
                  Clear selection
                </button>
              </div>
            )}

            {editing ? (
              <textarea
                className="wb-textarea mono"
                value={draft}
                onChange={(e) => setDraft(e.target.value)}
                spellCheck={false}
                aria-label={`Editor for ${fileView.path}`}
              />
            ) : (
              <div className="wb-codebox" role="region" aria-label={`Source of ${fileView.path}`}>
                <table className="wb-codetable">
                  <tbody>
                    {lines.map((line, idx) => {
                      const lineNum = idx + 1;
                      const isSelected =
                        selectedRange &&
                        lineNum >= selectedRange[0] &&
                        lineNum <= selectedRange[1];
                      const isMatch = searchTerm && line.toLowerCase().includes(searchTerm.toLowerCase());

                      return (
                        <tr
                          key={idx}
                          className={`wb-codeline ${isSelected ? "wb-line-selected" : ""} ${
                            isMatch ? "wb-line-match" : ""
                          }`}
                        >
                          <td
                            className="wb-lineno"
                            onClick={() => handleLineClick(lineNum)}
                            title="Click to select line range for Agent"
                          >
                            {lineNum}
                          </td>
                          <td className="wb-linetext">
                            <HighlightedLine line={line} path={fileView.path} />
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            )}
          </>
        )}
      </div>
    </div>
  );
}
