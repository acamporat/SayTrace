!macro NSIS_HOOK_PREINSTALL
  nsExec::ExecToStack '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -NonInteractive -Command "if (Get-Process -Name local-transcript,local-transcript-worker -ErrorAction SilentlyContinue) { exit 23 }"'
  Pop $0
  Pop $1
  StrCmp $0 "0" local_transcript_preinstall_clear
  StrCmp $0 "23" 0 local_transcript_preinstall_check_failed
    MessageBox MB_ICONSTOP|MB_OK "Close SayTrace and wait for its local worker to stop before installing or updating the application."
    Abort
  local_transcript_preinstall_check_failed:
    MessageBox MB_ICONSTOP|MB_OK "Setup could not verify that SayTrace is closed. Close the application and its local worker, then try again."
    Abort
  local_transcript_preinstall_clear:
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::ExecToStack '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -NonInteractive -Command "if (Get-Process -Name local-transcript,local-transcript-worker -ErrorAction SilentlyContinue) { exit 23 }"'
  Pop $0
  Pop $1
  StrCmp $0 "0" local_transcript_preuninstall_clear
  StrCmp $0 "23" 0 local_transcript_preuninstall_check_failed
    MessageBox MB_ICONSTOP|MB_OK "Close SayTrace and wait for its local worker to stop before uninstalling the application."
    Abort
  local_transcript_preuninstall_check_failed:
    MessageBox MB_ICONSTOP|MB_OK "Setup could not verify that SayTrace is closed. Close the application and its local worker, then try again."
    Abort
  local_transcript_preuninstall_clear:
!macroend
