"""Sends a simulated trip-planning run to a running tracker.

    python clients/python/examples/demo.py
"""

import asyncio
import os
import random
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from agent_tracker import Tracker  # noqa: E402

t = Tracker()
SPEED = float(os.environ.get("DEMO_SPEED", "1"))
MODEL = "claude-sonnet-5-5"


async def sleep(ms: float) -> None:
    await asyncio.sleep(ms * random.uniform(0.75, 1.25) / SPEED / 1000)


async def plan_day(day: int, theme: str) -> str:
    with t.agent(f"day-planner-{day}", input=theme):
        with t.tool("search_places", input={"city": "Kyoto", "theme": theme}) as s:
            await sleep(400 + 150 * day)
            s.output([f"{theme} spot {i}" for i in range(1, 4)])
        with t.llm("draft itinerary", model=MODEL) as s:
            await sleep(800)
            s.tokens(900, 180).output(f"Day {day}: {theme} — 3 stops, walking route")
        return f"Day {day}: {theme}"


async def main() -> None:
    prompt = "Plan a 3-day trip to Kyoto: temples, food, and a day trip."
    with t.trace("3-day Kyoto itinerary", input=prompt):
        with t.agent("planner", input=prompt):
            with t.llm("choose themes", model=MODEL) as s:
                await sleep(700)
                themes = ["temples", "food markets", "Nara day trip"]
                s.tokens(320, 40).output(themes)
            with t.split("one planner per day", input=themes):
                days = await asyncio.gather(*(plan_day(i + 1, th) for i, th in enumerate(themes)))
            with t.join("assemble itinerary", input=days) as s:
                s.output("\n".join(days))
    t.flush()
    print(f"done - open {t.url}")


asyncio.run(main())
