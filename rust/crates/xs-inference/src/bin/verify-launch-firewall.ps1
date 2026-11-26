$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
try {
    $programs = ConvertFrom-Json $env:XUNSHU_LAUNCH_PROGRAMS
    $group = 'Xunshu Offline'
    $policy = New-Object -ComObject HNetCfg.FwPolicy2
    if ($policy.LocalPolicyModifyState -eq 1) {
        throw '组策略覆盖了本地离线规则，请联系系统管理员。'
    }
    foreach ($profile in 1, 2, 4) {
        if (!$policy.FirewallEnabled($profile)) {
            throw 'Windows 防火墙配置未启用，请联系系统管理员。'
        }
        if (!$policy.IsRuleGroupEnabled($profile, $group)) {
            throw '寻枢离线规则组已停用，请使用安装包修复。'
        }
    }
    $eligible = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($rule in $policy.Rules) {
        if ($rule.Grouping -ne $group -or $rule.ApplicationName -notin $programs) { continue }
        # NET_FW_RULE_DIR_OUT=2，NET_FW_ACTION_BLOCK=0，NET_FW_IP_PROTOCOL_ANY=256。
        if ($rule.Enabled -and $rule.Direction -eq 2 -and $rule.Action -eq 0 -and
            ($rule.Profiles -band 7) -eq 7 -and $rule.Protocol -eq 256 -and
            $rule.LocalAddresses -eq '*' -and $rule.RemoteAddresses -eq '*' -and
            $rule.InterfaceTypes -eq 'All' -and !$rule.Interfaces -and !$rule.ServiceName) {
            [void]$eligible.Add($rule.ApplicationName)
        }
    }
    foreach ($program in $programs) {
        if (!$eligible.Contains($program)) {
            throw ('离线规则缺失或范围不完整：' + [IO.Path]::GetFileName($program) + '。请使用安装包修复。')
        }
    }
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}
