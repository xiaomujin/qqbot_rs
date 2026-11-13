# 发布：构建 release → 部署到 run\ → 重启（发布环境，独立目录）
#
# 用法：.\deploy.ps1            构建 + 部署 + 重启
#       .\deploy.ps1 -NoRestart  构建 + 部署，但**不启动**（实例会先停掉，等你手动起）
#
# 用 pwsh（PowerShell 7）跑最好；用 Windows PowerShell 5.1 跑也能跑完。
#
# 为什么要脚本：这几步每一步都能**悄悄**出错，而且出错时看起来都像「没问题」——
#   · 拷错二进制：把 debug 拷进去，大卡片渲染 3.5 秒 → 顶破 5 秒预算 → 降级成文字
#   · 旧实例没退干净：新进程起不来，或者两个实例同时在跑 → 同一条消息回两遍
#   · 配置没带：run\ 缺 config.toml 时 discover 会**向上**找到项目根那份，
#     「独立目录」当场变成假的，而启动照样成功
#
# 2026-11 修过的两个坑（都是「线上白停了一次」换来的）：
#
# 1. 模块自动加载不可靠：有一次在某个宿主里 Get-FileHash（Microsoft.PowerShell.Utility）
#    报 CommandNotFoundException，而它出现在第 42 行 —— **实例已经停掉之后**。
#    现在哈希走 [System.Security.Cryptography.SHA256]，纯 .NET，任何宿主都有；
#    另外检测到是 5.1 且机器上有 pwsh 时，脚本会**把自己重新拉进 pwsh**。
# 2. 停服务太早：原来「停实例 → 拷临时文件 → 比哈希 → 替换」，任何一步失败都留下
#    「已经停服、也没换上新二进制」。现在临时文件与哈希校验都在停服务**之前**做完，
#    停服之后只剩一次原子 Move。
# 3. 没有 MSVC 环境：这一条 2026-11 才暴露 —— 前几次发布都「构建空转」（我提前编过），
#    看起来一切正常；第一次真的需要重编，`cargo build --release` 直接报
#    `linker link.exe not found`（普通 PowerShell 窗口的 PATH 里没有 MSVC）。
#    现在脚本自己把 vcvars64.bat 的环境**导入当前进程**，缺 link.exe 才继续往下走。
param([switch]$NoRestart)

# 能用 pwsh 就切过去：两代 PowerShell 的模块解析规则不同，脚本自身的环境要一致。
# 用环境变量做哨兵，避免 pwsh 又把自己当成 5.1 再拉一次。
if ($PSVersionTable.PSEdition -eq 'Desktop' -and -not $env:QQBOT_DEPLOY_REEXEC) {
    $pwsh = Get-Command pwsh -ErrorAction SilentlyContinue
    if ($pwsh) {
        $env:QQBOT_DEPLOY_REEXEC = '1'
        & $pwsh.Source -NoProfile -ExecutionPolicy Bypass -File $PSCommandPath @args
        exit $LASTEXITCODE
    }
}

$ErrorActionPreference = 'Stop'

# 前置检查：**在碰线上之前**确认工具箱齐全。
# 一次「停完才发现命令不存在」= 一次事故，而这里一条 cmdlet 就能挡住。
$required = 'cargo', 'Get-Process', 'Stop-Process', 'Start-Process', 'Copy-Item', 'Move-Item',
    'New-Item', 'Test-Path', 'Get-Content', 'Join-Path'
$missing = @($required | Where-Object { -not (Get-Command $_ -ErrorAction SilentlyContinue) })
if ($missing.Count -gt 0) {
    throw "当前宿主缺少这些命令：$($missing -join '、')。请改用 pwsh 运行：pwsh -File .\deploy.ps1"
}

# 纯 .NET 的 SHA256（不依赖任何模块；Convert.ToHexString 在 .NET Framework 上没有，故手工拼）。
function Get-FileSha256([string]$Path) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $fs = [System.IO.File]::OpenRead($Path)
        try {
            $bytes = $sha.ComputeHash($fs)
            return (($bytes | ForEach-Object { $_.ToString('x2') }) -join '')
        }
        finally { $fs.Dispose() }
    }
    finally { $sha.Dispose() }
}

# 链接器：release 构建走 MSVC 目标，必须有 link.exe。普通 PowerShell 窗口的 PATH 里
# **没有它**（那是 VS 开发者命令行才有的），所以只要这次真有东西要链接就会炸在
# `linker link.exe not found` 上 —— 而「空转构建」的发布看不出这个洞。
# 处理办法：把 vcvars64.bat 的环境**导入当前进程**（比 re-exec 干净，不用再传一遍参数）。
if (-not (Get-Command link.exe -ErrorAction SilentlyContinue)) {
    $vcvars = $null
    $vswhere = @('C:\Program Files (x86)', 'C:\Program Files') |
        ForEach-Object { Join-Path $_ 'Microsoft Visual Studio\Installer\vswhere.exe' } |
        Where-Object { Test-Path $_ } |
        Select-Object -First 1
    if ($vswhere) {
        $install = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if ($install) { $vcvars = Join-Path $install.Trim() 'VC\Auxiliary\Build\vcvars64.bat' }
    }
    if (-not $vcvars -or -not (Test-Path $vcvars)) {
        # vswhere 也没有（VS 装法不同）：直接在常见目录里扫一遍。
        $vcvars = Get-ChildItem 'C:\Program Files\Microsoft Visual Studio\*\*\VC\Auxiliary\Build\vcvars64.bat' -ErrorAction SilentlyContinue |
            Select-Object -First 1 -ExpandProperty FullName
    }
    if (-not $vcvars) {
        throw '找不到 link.exe，也没找到 vcvars64.bat。请从「x64 Native Tools Command Prompt for VS」启动，或先手动 call 一次 vcvars64.bat。'
    }
    # 「call vcvars && set」的输出就是完整的环境变量表，逐行写回本进程。
    $dump = cmd /c "call `"$vcvars`" >nul 2>&1 && set"
    foreach ($line in $dump) {
        if ($line -match '^([^=]+)=(.*)$') {
            Set-Item -Path ("env:" + $matches[1]) -Value $matches[2]
        }
    }
    if (-not (Get-Command link.exe -ErrorAction SilentlyContinue)) {
        throw "导入了 $vcvars 的环境，但仍然找不到 link.exe"
    }
    Write-Output "已导入 MSVC 环境（$vcvars）"
}

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

# 先把新二进制拷到临时文件并校验，**仍然不动线上**。
$tmp = "$exe.new"
Copy-Item -Force $built $tmp
if ((Get-FileSha256 $tmp) -ne (Get-FileSha256 $built)) {
    Remove-Item -Force $tmp -ErrorAction SilentlyContinue
    throw '二进制没拷成功（哈希不一致），线上未受影响'
}

# 到这里才开始停实例。Windows 锁住正在运行的 exe，替换必须先停。
# 同一 AppID 只能有一个实例在线，所以「全停」不是保守，是正确性。
Get-Process qqbot -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2
if (@(Get-Process qqbot -ErrorAction SilentlyContinue).Count -ne 0) { throw '旧实例没退干净' }

# 原子替换：同盘内 Move 是改名，不会留下半个二进制。
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
