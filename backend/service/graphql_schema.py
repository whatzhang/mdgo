from __future__ import annotations

import strawberry
from datetime import datetime, timezone


@strawberry.type
class HeartbeatResult:
    status: str
    timestamp: str
    service: str


@strawberry.type
class Query:
    @strawberry.field
    def heartbeat(self) -> HeartbeatResult:
        return HeartbeatResult(
            status="ok",
            timestamp=datetime.now(timezone.utc).isoformat(),
            service="mdgo",
        )


schema = strawberry.Schema(query=Query)
