; Compiler-only fixture. Real release builds generate this macro file from the
; complete, hashed payload in a uniquely named temporary staging directory.
!macro InstallRuntimePayload
  SetOutPath "$INSTDIR\smoke"
  File "/oname=revisions.json" "${SMOKE_FILE}"
!macroend

!macro UninstallRuntimePayload
  Delete "$INSTDIR\smoke\revisions.json"
  RMDir "$INSTDIR\smoke"
!macroend
