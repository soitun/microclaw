[CmdletBinding()]
param(
    [string]$Version,
    [string]$Repository,
    [string]$Sha = "HEAD",
    [switch]$Wait,
    [int]$CiTimeoutMinutes = 30
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Invoke-Checked {
    param([Parameter(Mandatory)][string[]]$Arguments)

    & $Arguments[0] $Arguments[1..($Arguments.Count - 1)]
    if ($LASTEXITCODE -ne 0) {
        throw "Command failed ($LASTEXITCODE): $($Arguments -join ' ')"
    }
}

function Get-LatestWorkflowRun {
    param(
        [Parameter(Mandatory)][string]$Workflow,
        [string]$Commit
    )

    $args = @("run", "list", "--repo", $Repository, "--workflow", $Workflow, "--limit", "20", "--json", "databaseId,headSha,status,conclusion,createdAt,url")
    $runs = & gh @args | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) {
        throw "Unable to list workflow runs for $Workflow"
    }
    if ($Commit) {
        return $runs | Where-Object { $_.headSha -eq $Commit } | Select-Object -First 1
    }
    return $runs | Select-Object -First 1
}

function Wait-ForCi {
    param([Parameter(Mandatory)][string]$Commit)

    $deadline = (Get-Date).AddMinutes($CiTimeoutMinutes)
    while ((Get-Date) -lt $deadline) {
        $run = Get-LatestWorkflowRun -Workflow "ci.yml" -Commit $Commit
        if ($null -eq $run) {
            Write-Host "Waiting for CI run for $Commit ..."
        } elseif ($run.status -eq "completed") {
            if ($run.conclusion -ne "success") {
                throw "CI did not pass: $($run.url) ($($run.conclusion))"
            }
            Write-Host "CI passed: $($run.url)"
            return
        } else {
            Write-Host "Waiting for CI: $($run.url) ($($run.status))"
        }
        Start-Sleep -Seconds 15
    }
    throw "Timed out waiting for CI after $CiTimeoutMinutes minutes"
}

foreach ($command in @("git", "gh")) {
    if (-not (Get-Command $command -ErrorAction SilentlyContinue)) {
        throw "Required command is not available: $command"
    }
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Push-Location $repoRoot
try {
    if (git status --porcelain) {
        throw "The worktree must be clean before a release"
    }

    $resolvedSha = (git rev-parse $Sha).Trim()
    if ($LASTEXITCODE -ne 0 -or -not $resolvedSha) {
        throw "Unable to resolve release commit: $Sha"
    }

    if (-not $Repository) {
        $Repository = (& gh repo view --json nameWithOwner --jq '.nameWithOwner').Trim()
        if ($LASTEXITCODE -ne 0 -or -not $Repository) {
            throw "Unable to determine GitHub repository; pass -Repository owner/name"
        }
    }

    $manifest = Get-Content -LiteralPath "Cargo.toml" -Raw
    $match = [regex]::Match($manifest, '(?m)^version\s*=\s*"([^"]+)"')
    if (-not $match.Success) {
        throw "Unable to read package version from Cargo.toml"
    }
    $manifestVersion = $match.Groups[1].Value
    if (-not $Version) {
        $Version = $manifestVersion
    }
    $Version = $Version.TrimStart('v')
    if ($Version -notmatch '^\d+\.\d+\.\d+([-.+][0-9A-Za-z.-]+)?$') {
        throw "Invalid semantic version: $Version"
    }
    if ($Version -ne $manifestVersion) {
        throw "Requested version $Version does not match Cargo.toml version $manifestVersion"
    }
    $tag = "v$Version"

    Invoke-Checked @("git", "fetch", "origin", "main", "--tags")
    & git merge-base --is-ancestor $resolvedSha origin/main
    if ($LASTEXITCODE -ne 0) {
        throw "Release commit $resolvedSha is not present on origin/main"
    }

    Invoke-Checked @("gh", "auth", "status")
    Wait-ForCi -Commit $resolvedSha

    & gh api "repos/$Repository/git/ref/tags/$tag" *> $null
    if ($LASTEXITCODE -eq 0) {
        Write-Host "Tag already exists: $tag"
    } else {
        Write-Host "Creating $tag at $resolvedSha ..."
        Invoke-Checked @("gh", "workflow", "run", "tag-release.yml", "--repo", $Repository, "-f", "tag=$tag", "-f", "sha=$resolvedSha")
        Start-Sleep -Seconds 3
        $tagRun = Get-LatestWorkflowRun -Workflow "tag-release.yml"
        if ($null -eq $tagRun) {
            throw "Tag workflow was dispatched but its run could not be found"
        }
        Invoke-Checked @("gh", "run", "watch", "$($tagRun.databaseId)", "--repo", $Repository, "--exit-status")
    }

    Write-Host "Triggering multi-platform assets for $tag ..."
    Invoke-Checked @("gh", "workflow", "run", "release-assets.yml", "--repo", $Repository, "-f", "tag=$tag")
    Start-Sleep -Seconds 3
    $releaseRun = Get-LatestWorkflowRun -Workflow "release-assets.yml"
    if ($null -eq $releaseRun) {
        throw "Release workflow was dispatched but its run could not be found"
    }

    Write-Host "Release workflow: $($releaseRun.url)"
    if ($Wait) {
        Invoke-Checked @("gh", "run", "watch", "$($releaseRun.databaseId)", "--repo", $Repository, "--exit-status")
        Invoke-Checked @("gh", "release", "view", $tag, "--repo", $Repository)
    } else {
        Write-Host "Use -Wait to wait for all Windows, macOS, Linux, container, and release assets."
    }
} finally {
    Pop-Location
}
