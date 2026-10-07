# Run Anchor on Windows with free local models (Ollama), in one command:
#
#   powershell -ExecutionPolicy Bypass -File scripts\try-local.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\try-local.ps1 -Workspace C:\my\project -Task "Fix the failing test"
#
# It builds Anchor if needed, checks Ollama, downloads the two models in
# examples\local\roles.ollama.toml, creates a test project (C:\anchor-test)
# unless you name one, and starts the agent. The agent asks before every
# write or command; answer y or n.
param(
    [string]$Workspace = "C:\anchor-test",
    [string]$Task = "Add a README.md that explains what this project does"
)
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

function Fail($msg) { Write-Host "`n$msg" -ForegroundColor Red; exit 1 }
function Step($msg) { Write-Host "`n== $msg" -ForegroundColor Cyan }

Step "Anchor"
$anchor = ".\target\debug\anchor.exe"
if (-not (Test-Path $anchor)) {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Fail "Rust is not installed. Install it from https://rustup.rs, open a new PowerShell window, and run this again."
    }
    cargo build
    if ($LASTEXITCODE -ne 0) { Fail "cargo build failed (see above)." }
}
Write-Host "ok: $anchor"

Step "Ollama"
if (-not (Get-Command ollama -ErrorAction SilentlyContinue)) {
    Fail "Ollama is not installed. Install it from https://ollama.com/download, open a new PowerShell window, and run this again."
}
function OllamaUp {
    try { Invoke-RestMethod -Uri "http://127.0.0.1:11434/api/tags" -TimeoutSec 3 | Out-Null; return $true }
    catch { return $false }
}
if (-not (OllamaUp)) {
    Write-Host "starting the Ollama server..."
    Start-Process ollama -ArgumentList "serve" -WindowStyle Hidden
    for ($i = 0; $i -lt 30 -and -not (OllamaUp); $i++) { Start-Sleep -Seconds 1 }
    if (-not (OllamaUp)) { Fail "Ollama is installed but its server did not start. Open the Ollama app, then run this again." }
}
Write-Host "ok: Ollama is running"

Step "Models (the first download is a few GB and takes a while)"
foreach ($m in @("qwen2.5-coder:7b", "llama3.1:8b")) {
    ollama pull $m
    if ($LASTEXITCODE -ne 0) { Fail "Could not download $m (see above)." }
}

Step "Project"
if (-not (Test-Path $Workspace)) {
    New-Item -ItemType Directory -Force $Workspace | Out-Null
    Set-Content -Encoding ascii (Join-Path $Workspace "hello.py") "print('hello')"
    Write-Host "created a test project: $Workspace"
} else {
    Write-Host "using: $Workspace"
}

Step "Running the agent"
& $anchor run --workspace $Workspace --roles examples\local\roles.ollama.toml --task $Task
Write-Host "`nTo undo a turn:  $anchor checkpoint list --workspace $Workspace"
Write-Host "                 $anchor rollback --workspace $Workspace --turn N"
