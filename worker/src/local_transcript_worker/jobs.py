"""Single-GPU asynchronous job queue with cancellation."""

from __future__ import annotations

import queue
import threading
from collections.abc import Callable
from dataclasses import dataclass

from .errors import ErrorCode, WorkerError
from .schema import JsonObject

JobCallable = Callable[[threading.Event], JsonObject]
EventCallback = Callable[[str, JsonObject], None]


@dataclass(slots=True)
class _Job:
    job_id: str
    run: JobCallable
    cancel: threading.Event


class JobManager:
    def __init__(self, emit: EventCallback) -> None:
        self.emit = emit
        self._queue: queue.Queue[_Job | None] = queue.Queue()
        self._jobs: dict[str, _Job] = {}
        self._lock = threading.Lock()
        self._thread = threading.Thread(target=self._worker, name="gpu-job-queue", daemon=True)
        self._thread.start()

    def submit(self, job_id: str, run: JobCallable) -> None:
        with self._lock:
            if job_id in self._jobs:
                raise WorkerError(ErrorCode.BAD_REQUEST, "Job ID is already active.")
            job = _Job(job_id, run, threading.Event())
            self._jobs[job_id] = job
            self._queue.put(job)

    def cancel(self, job_id: str) -> bool:
        with self._lock:
            job = self._jobs.get(job_id)
            if job is None:
                return False
            job.cancel.set()
            return True

    def status(self) -> JsonObject:
        with self._lock:
            return {
                "active_job_ids": sorted(self._jobs),
                "queued_jobs": self._queue.qsize(),
            }

    def close(self) -> None:
        with self._lock:
            for job in self._jobs.values():
                job.cancel.set()
        self._queue.put(None)
        self._thread.join(timeout=10)

    def _worker(self) -> None:
        while True:
            job = self._queue.get()
            if job is None:
                return
            self.emit("job_started", {"job_id": job.job_id})
            try:
                if job.cancel.is_set():
                    raise WorkerError(ErrorCode.CANCELLED, "Job was cancelled before it started.")
                result = job.run(job.cancel)
                self.emit("job_complete", {"job_id": job.job_id, "result": result})
            except WorkerError as exc:
                self.emit("job_error", {"job_id": job.job_id, "error": exc.as_dict()})
            except Exception as exc:
                self.emit(
                    "job_error",
                    {
                        "job_id": job.job_id,
                        "error": {
                            "code": ErrorCode.INTERNAL.value,
                            "message": "Unhandled pipeline failure.",
                            "retryable": False,
                            "details": {"exception": type(exc).__name__},
                        },
                    },
                )
            finally:
                with self._lock:
                    self._jobs.pop(job.job_id, None)
