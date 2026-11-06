# 测试：构建 debug → 从副本重启（测试环境，cwd = 项目根，用自己的 config.toml 与 data\）
#
# 用法：.\dev.ps1
#
# 两件事必须一起做，否则「测试」会变成线上事故：
#   1. 从 target\devrun\qqbot.exe 启动 —— Windows 锁住正在运行的 exe，
#      直接跑 target\debug\qqbot.exe 会让 cargo 无法重链（os error 5）。
#   2. 先停掉发布实例 —— 同一个 AppID 两个实例在线，同一条消息会被回两遍。
#      所以测试期间线上是停着的；要一边测一边正常服务，得用两个 AppID。
param()

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$dev = Join-Path $root 'target\devrun'
$built = Join-Path $root 'target\debug\qqbot.exe'

cargo build --manifest-path (Join-Path $root 'Cargo.toml')
if ($LASTEXITCODE -ne 0) { throw 'cargo build 失败' }

New-Item -ItemType Directory -Force -Path $dev | Out-Null
$exe = Join-Path $dev 'qqbot.exe'
Copy-Item -Force $built $exe
if ((Get-FileHash $built).Hash -ne (Get-FileHash $exe).Hash) {
    throw '二进制没换成功：旧实例还在跑？'
}

Get-Process qqbot -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2
if (@(Get-Process qqbot -ErrorAction SilentlyContinue).Count -ne 0) { throw '旧实例没退干净' }

$log = Join-Path $root 'target\live.log'
Remove-Item $log -ErrorAction SilentlyContinue
$env:RUST_LOG = 'info'
$start = @{
    FilePath               = $exe
    ArgumentList           = 'run'
    WorkingDirectory       = $root
    RedirectStandardOutput = $log
    RedirectStandardError  = (Join-Path $root 'target\live.err.log')
    WindowStyle            = 'Hidden'
}
Start-Process @start
Start-Sleep -Seconds 6

Get-Process qqbot | Select-Object Id, Path | Format-Table -AutoSize
Get-Content $log -Tail 4
Write-Output "测试实例日志：$log（Get-Content $log -Wait 跟踪）"
