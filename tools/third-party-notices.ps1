<#
.SYNOPSIS
Write THIRD-PARTY-NOTICES.txt: every crate compiled into the tmiki executable,
with its license expression and the license / notice files it ships (MIT,
BSD and Apache-2.0 ask for those to travel with binaries).

.DESCRIPTION
The crate list is `cargo tree -p tmi-cli -e normal` for the target; the files
come from each crate's directory in the cargo registry (or the repository, for
vendored code). tmiki's own crates are left out. A -sys crate also gets the
terms of the C library it bundles (files below its directory).

.EXAMPLE
pwsh tools/third-party-notices.ps1 -Out dist/THIRD-PARTY-NOTICES.txt
#>
param(
    [string]$Target = 'x86_64-pc-windows-msvc',
    [string]$Out
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$rust = Join-Path $root 'rust'
if (-not $Out) { $Out = Join-Path $root 'THIRD-PARTY-NOTICES.txt' }
$names = @('license', 'licence', 'copying', 'notice', 'unlicense', 'authors')

Push-Location $rust
try {
    $tree = & cargo tree -p tmi-cli -e normal --target $Target --prefix none -f '{p}'
    if ($LASTEXITCODE -ne 0) { throw 'cargo tree failed' }
    $meta = (& cargo metadata --format-version 1 --locked) -join "`n" | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed' }
} finally {
    Pop-Location
}

$members = @{}
foreach ($m in $meta.workspace_members) { $members[$m] = $true }
$byKey = @{}
foreach ($p in $meta.packages) { $byKey["$($p.name) $($p.version)"] = $p }

$seen = [System.Collections.Generic.SortedSet[string]]::new([StringComparer]::Ordinal)
foreach ($line in $tree) {
    $parts = $line -split '\s+'
    if ($parts.Count -ge 2 -and $parts[1].StartsWith('v')) {
        [void]$seen.Add("$($parts[0]) $($parts[1].Substring(1))")
    }
}

function Test-NoticeName([string]$name, [string[]]$prefixes) {
    $lower = $name.ToLowerInvariant()
    foreach ($x in $prefixes) { if ($lower.StartsWith($x)) { return $true } }
    return $false
}

$sb = [System.Text.StringBuilder]::new()
[void]$sb.Append("Third-party software in tmiki`n=============================`n`n")
[void]$sb.Append("The tmiki executable contains the following crates. Each is listed with`n")
[void]$sb.Append("its license expression and the license and notice files it ships.`n")
$missing = [System.Collections.Generic.List[string]]::new()
$count = 0
foreach ($key in $seen) {
    $p = $byKey[$key]
    if ($null -eq $p) { continue }
    $vendored = ($p.manifest_path -replace '\\', '/') -match '/vendor/'
    if ($members.ContainsKey($p.id) -and -not $vendored) { continue }
    $count++
    $d = Split-Path $p.manifest_path -Parent
    $files = [System.Collections.Generic.List[string]]::new()
    Get-ChildItem -LiteralPath $d -File | Where-Object { Test-NoticeName $_.Name $names } |
        Sort-Object Name -CaseSensitive | ForEach-Object { $files.Add($_.FullName) }
    if ($p.license_file) {
        $lf = Join-Path $d $p.license_file
        if ((Test-Path -LiteralPath $lf -PathType Leaf) -and -not $files.Contains($lf)) { $files.Add($lf) }
    }
    if ($p.name.EndsWith('-sys')) {
        Get-ChildItem -LiteralPath $d -File -Recurse -Depth 2 |
            Where-Object { $_.DirectoryName -ne $d -and (Test-NoticeName $_.Name ($names + 'patents')) } |
            Sort-Object FullName | ForEach-Object { $files.Add($_.FullName) }
    }
    $license = if ($p.license) { $p.license } else { '(see file)' }
    [void]$sb.Append("`n$('-' * 78)`n$($p.name) $($p.version)`nLicense: $license`n")
    if ($p.repository) { [void]$sb.Append("Source: $($p.repository)`n") }
    if ($files.Count -eq 0) {
        $missing.Add("$($p.name) $($p.version)")
        [void]$sb.Append("(no license file in the package; the license expression above applies)`n")
    }
    foreach ($f in $files) {
        $rel = [IO.Path]::GetRelativePath($d, $f) -replace '\\', '/'
        $text = [IO.File]::ReadAllText($f).TrimEnd()
        [void]$sb.Append("`n--- $rel ---`n$text`n")
    }
}
$textOut = $sb.ToString() -replace "`r`n", "`n"
[IO.File]::WriteAllText($Out, $textOut, [Text.UTF8Encoding]::new($false))
"wrote $Out ($count crates, $([math]::Floor((Get-Item $Out).Length / 1KB)) KB)"
if ($missing.Count) { 'without a license file: ' + ($missing -join ', ') }
