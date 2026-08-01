!macro EnsureLocalTranscriptIdle
  nsExec::ExecToStack '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -NonInteractive -Command "if (Get-Process -Name local-transcript,local-transcript-worker -ErrorAction SilentlyContinue) { exit 23 }"'
  Pop $0
  Pop $1
  StrCmp $0 "0" local_transcript_idle
  StrCmp $0 "23" 0 local_transcript_idle_check_failed
    MessageBox MB_ICONSTOP|MB_OK "Close SayTrace and wait for its local worker to stop before continuing."
    Abort
  local_transcript_idle_check_failed:
    MessageBox MB_ICONSTOP|MB_OK "Setup could not verify that SayTrace is closed. Close the application and its local worker, then try again."
    Abort
  local_transcript_idle:
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro EnsureLocalTranscriptIdle
  IfFileExists "$EXEDIR\runtime\runtime-manifest.json" +3 0
    MessageBox MB_ICONSTOP|MB_OK "The SayTrace setup payload is incomplete. Download the complete setup again."
    Abort
  IfFileExists "$EXEDIR\install-runtime.ps1" +3 0
    MessageBox MB_ICONSTOP|MB_OK "The SayTrace runtime installer is missing. Download the complete setup again."
    Abort
!macroend

!macro NSIS_HOOK_POSTINSTALL
  nsExec::ExecToStack '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$EXEDIR\install-runtime.ps1" -SourceRuntime "$EXEDIR\runtime" -DestinationRuntime "$INSTDIR\runtime"'
  Pop $0
  Pop $1
  StrCmp $0 "0" local_transcript_runtime_installed
    MessageBox MB_ICONSTOP|MB_OK "SayTrace was installed, but its offline processing runtime could not be verified and added. Setup has stopped. Run the complete setup again. Details: $1"
    Abort
  local_transcript_runtime_installed:
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro EnsureLocalTranscriptIdle
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  RMDir /r "$INSTDIR\runtime"
!macroend
