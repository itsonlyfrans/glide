; Custom steps for the Windows installer.
; The always-on engine (glided.exe) must be stopped before its file can be replaced or removed.
!macro StopGlideEngine
  nsExec::Exec 'taskkill /IM glided.exe'
  Sleep 1000
  nsExec::Exec 'taskkill /F /IM glided.exe'
  Sleep 300
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro StopGlideEngine
  ; Glide used to be an Electron app installed under Programs\Glide. Remove that copy quietly; settings and pairings
  ; live in %APPDATA%\Glide and are kept.
  IfFileExists "$LOCALAPPDATA\Programs\Glide\Uninstall Glide.exe" 0 +3
    ExecWait '"$LOCALAPPDATA\Programs\Glide\Uninstall Glide.exe" /S _?=$LOCALAPPDATA\Programs\Glide'
    RMDir /r "$LOCALAPPDATA\Programs\Glide"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro StopGlideEngine
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Glide"
!macroend
