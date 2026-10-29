import { useCallback, useEffect, useState } from "react";
import { get, post, q } from "../api";
import type { ClaimsView, WorkspaceFileView } from "../types";
import { AgentChip, Badge, Empty, ErrorLine } from "../util";

export function CodeWorkbench({
  version,
  colors,
  selectedFile,
  onSelectFile,
  compact = false,
}: {
  version: number;
  colors: (n: string | null | undefined) => string;
  selectedFile: string | null;
  onSelectFile: (path: string) => void;
  compact?: boolean;
}) {
  const [files, setFiles] = useState<string[]>([]);
  const [filter, setFilter] = useState("");
  const [fileView, setFileView] = useState<WorkspaceFileView | null>(null);
  const [draft, setDraft] = useState("");
  const [editing, setEditing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [statusMsg, setStatusMsg] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    get<ClaimsView>("/api/claims").then(
      (v) => {
        setFiles(v.files);
        if (!selectedFile && v.files.length > 0) {
          onSelectFile(v.files[0]!);
        }
      },
      (e) => setError(String(e)),
    );
  }, [version, selectedFile, onSelectFile]);

  const loadFile = useCallback(async (path: string) => {
    try {
      const v = await get<WorkspaceFileView>(`/api/workspace-file${q({ path })}`);
      setFileView(v);
      setDraft(v.content);
      setEditing(false);
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    if (selectedFile) {
      void loadFile(selectedFile);
    }
  }, [selectedFile, version, loadFile]);

  const saveEdit = async () => {
    if (!fileView) return;
    setSaving(true);
    setStatusMsg(null);
    try {
      await post("/api/file/edit", { path: fileView.path, content: draft });
      setStatusMsg(`Saved ${fileView.path} (snapshot recorded in audit log)`);
      await loadFile(fileView.path);
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const filteredFiles = files.filter((f) => !filter || f.toLowerCase().includes(filter.toLowerCase()));
  const lines = (editing ? draft : (fileView?.content ?? "")).split("\n");

  return (
    <div className={compact ? "wb-compact" : "wb-split"}>
      {!compact && (
        <div className="wb-sidebar">
          <div className="wb-section-head">
            <span>WORKSPACE FILES</span>
            <span className="mono muted">{files.length}</span>
          </div>
          <input
            type="search"
            className="wb-filter"
            placeholder="Filter path…"
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
                  className={f === selectedFile ? "wb-filebtn wb-filebtn-on" : "wb-filebtn"}
                  onClick={() => onSelectFile(f)}
                >
                  <code>{f}</code>
                </button>
              </li>
            ))}
          </ul>
        </div>
      )}

      <div className="wb-editor-pane">
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
              <div className="row small mono muted">
                <span>{fileView.lines} LOC</span>
                <span>·</span>
                <span>{fileView.bytes} B</span>
                <span>·</span>
                <span title={`SHA-256: ${fileView.sha256}`}>sha256:{fileView.sha256.slice(0, 12)}</span>
                {fileView.editable && !editing && (
                  <button type="button" className="btn btn-small" onClick={() => setEditing(true)}>
                    Edit buffer
                  </button>
                )}
                {editing && (
                  <>
                    <button type="button" className="btn btn-small btn-primary" disabled={saving} onClick={saveEdit}>
                      {saving ? "Writing…" : "Commit write"}
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
              </div>
            </div>

            {editing ? (
              <textarea
                className="wb-textarea"
                value={draft}
                onChange={(e) => setDraft(e.target.value)}
                spellCheck={false}
                aria-label={`Editor for ${fileView.path}`}
              />
            ) : (
              <div className="wb-codebox" role="region" aria-label={`Source of ${fileView.path}`}>
                <table className="wb-codetable">
                  <tbody>
                    {lines.map((line, idx) => (
                      <tr key={idx} className="wb-codeline">
                        <td className="wb-lineno">{idx + 1}</td>
                        <td className="wb-linetext">{line || " "}</td>
                      </tr>
                    ))}
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
