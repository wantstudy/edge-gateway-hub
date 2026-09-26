; Tauri NSIS 安装钩子（由 tauri.conf.json 的 bundle.windows.nsis.installerHooks 引入）。
;
; 背景：历史版本把 `config.example.toml` 作为随包资源铺到安装目录（`$INSTDIR`）。
; 新版已不再分发该模板——首次运行的配置初始化改由壳内联生成最小 `config.toml`，
; 落在 `%APPDATA%\com.iotdaq.gateway\`（用户数据目录，非安装目录）。
;
; NSIS 升级安装只会覆盖/新增文件，**不会**删除上一版本遗留而新版不再分发的文件，
; 故在此显式清理，确保安装目录里不再暴露该模板文件。

!macro NSIS_HOOK_POSTINSTALL
  Delete "$INSTDIR\config.example.toml"
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  Delete "$INSTDIR\config.example.toml"
!macroend
