; Sentry NSIS installer hooks
; Called by the Tauri-generated NSIS installer at install/uninstall time.
; The installer runs with administrator privileges (installMode = perMachine).

!macro NSIS_HOOK_PREINSTALL
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Tear down any prior service registration (ignored on fresh install)
  ExecWait 'sc stop SentrySvc'
  ExecWait '"$INSTDIR\sentry-svc.exe" uninstall'

  ; Copy default config if one does not already exist.
  ; User must set claude_cli_path and user_profile before the service will work.
  IfFileExists "$INSTDIR\config.toml" +2
    CopyFiles /SILENT "$INSTDIR\config.toml.example" "$INSTDIR\config.toml"

  ; Register and start the service
  ExecWait '"$INSTDIR\sentry-svc.exe" install'
  ExecWait 'sc start SentrySvc'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; Stop and unregister before the installer removes the binary
  ExecWait 'sc stop SentrySvc'
  ExecWait '"$INSTDIR\sentry-svc.exe" uninstall'
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
!macroend
