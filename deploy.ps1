# 发布：构建 release → 部署到 run\ → 重启（发布环境，独立目录）
#
# 用法：.\deploy.ps1            构建 + 部署 + 重启
#       .\deploy.ps1 -NoRestart  构建 + 部署，但**不启动**（实例会先停掉，等你手动起）
#
# 为什么要脚本：这几步每一步都能**悄悄**出错，而且出错时看起来都像「没问题」——
#   · 拷错二进制：把 debug 拷进去，大卡片渲染 3.5 秒 → 顶破 5 秒预算 → 降级成文字
#   · 旧实例没退干净：新进程起不来，或者两个实例同时在跑 → 同一条消息回两遍
#   · 配置没带：run\ 缺 config.toml 时 discover 会**向上**找到项目根那份，
#     「独立目录」当场变成假的，而启动照样成功
param([switch]$NoRestart)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$run = Join-Path $root 'run'
$built = Join-Path $root 'target\release\qqbot.exe'

cargo build --release --manifest-path (Join-Path $root 'Cargo.toml')
if ($LASTEXITCODE -ne 0) { throw 'cargo build --release 失败' }

New-Item -ItemType Directory -Force -Path $run, (Join-Path $run 'data'), (Join-Path $run 'logs') | Out-Null
$exe = Join-Path $run 'qqbot.exe'

# 发布目录必须自带配置（首次：copy config.toml run\config.toml）。
# 放在停实例**之前**：配置有问题时不该让线上白停一次。
$cfg = Join-Path $run 'config.toml'
if (-not (Test-Path $cfg)) {
    throw "缺少 $cfg —— 发布目录要自带一份配置，不能靠向上找到项目根那份"
}

# **先停再拷**：Windows 锁住正在运行的 exe，先拷必然失败（原来的顺序就是这样，
# 于是「一条命令发布」在实例活着时根本跑不通）。停实例也不能省 —— 同一 AppID
# 只能有一个实例在线，发布与测试同时跑会让同一条消息回两遍。
Get-Process qqbot -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2
if (@(Get-Process qqbot -ErrorAction SilentlyContinue).Count -ne 0) { throw '旧实例没退干净' }

# 先写临时文件再原子替换：直接覆盖时若拷到一半失败，run\ 里会留下半个二进制，
# 而那时实例已经停了 —— 等于既停服又起不来。
$tmp = "$exe.new"
Copy-Item -Force $built $tmp
if ((Get-FileHash $tmp).Hash -ne (Get-FileHash $built).Hash) { throw '二进制没拷成功' }
Move-Item -Force $tmp $exe

if ($NoRestart) { Write-Output "已部署到 $run（实例已停止，未启动）"; return }

foreach ($f in 'live.log', 'live.err.log') {
    $p = Join-Path $run "logs\$f"
    if (Test-Path $p) { Move-Item -Force $p "$p.prev" }
}

$env:RUST_LOG = 'info'
$start = @{
    FilePath               = $exe
    ArgumentList           = 'run'
    WorkingDirectory       = $run
    RedirectStandardOutput = (Join-Path $run 'logs\live.log')
    RedirectStandardError  = (Join-Path $run 'logs\live.err.log')
    WindowStyle            = 'Hidden'
}
Start-Process @start
Start-Sleep -Seconds 6

# Start-Process 成功不代表进程活着（起不来会立刻退出），必须确认它还在
$procs = @(Get-Process qqbot -ErrorAction SilentlyContinue)
if ($procs.Count -eq 0) { throw "启动失败：没有 qqbot 进程，看 $(Join-Path $run 'logs\live.err.log')" }
$procs | Select-Object Id, Path | Format-Table -AutoSize
Get-Content (Join-Path $run 'logs\live.log') -Tail 4
