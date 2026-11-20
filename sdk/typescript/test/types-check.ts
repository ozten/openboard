// Compile-only: `tsc -p test` checks that the typings fit typical use.
import { board, createTasksInChunks, type BoardEvent } from "../src/index.js";

async function worker(): Promise<void> {
  const team = board().as("worker-1");
  const claim = await team.tasks.claim({ channel: "build/", label: { role: "worker" }, wait: "10m" });
  if (claim) {
    await team.reply(claim.task.id, "on it");
    await claim.renew("2m");
    const done = await claim.complete("tests pass");
    const state: string | undefined = done.task?.state;
    void state;
  }
  const lead = board({ profile: "lead" });
  const [fix] = await lead.tasks.createMany("build", [{ key: "fix", text: "fix it" }], { id: "p" });
  await lead.tasks.create("build", { text: "review", needs: [fix.id] });
  await createTasksInChunks(lead, "build", [{ text: "x" }]);
  const event: BoardEvent | null = await lead.wait({ label: "board.task=completed" }, { timeout: "30m" });
  void event;
  for await (const e of lead.events({ channel: "build" })) {
    void e.type;
  }
}
void worker;
