// Permission and elicitation cards: the fake agent gates its next update on the client's answer.

import { test, expect } from "../../helpers/liveTest";
import { chunk, endTurn, openStructuredView, script, startAcpSession } from "../../helpers/acp";

const turn = (...updates: object[]) => script(endTurn(...updates));
const permission = (toolCallId: string, title: string) => ({
  sessionUpdate: "permission_request",
  toolCall: { toolCallId, title, kind: "edit" },
});
const OPTION_NAMES = ["Option Alpha", "Option Bravo", "Option Charlie", "Option Delta"];

test("ApprovalCard Allow resolves and the turn continues", async ({ page, spawnServe }) => {
  const { serve, sessionId } = await startAcpSession(spawnServe, {
    title: "story-allow",
    fakeAcpScript: turn(
      chunk("About to write a file..."),
      permission("fake-tool-call-allow", "Write file"),
      chunk("Write complete."),
    ),
  });
  await openStructuredView(page, serve, sessionId, "please write something");

  const approvalDialog = page.getByRole("alertdialog", { name: /Approval needed/i });
  await expect(approvalDialog).toBeVisible({ timeout: 10_000 });
  // #2145: the agent is parked on the decision, so no working spinner.
  await expect(page.getByTestId("acp-working-spinner")).toHaveCount(0);
  const postApprovalChunk = page.getByText("Write complete.");
  await expect(postApprovalChunk).toHaveCount(0);

  await approvalDialog.getByRole("button", { name: "Allow" }).click();
  await expect(postApprovalChunk).toBeVisible({ timeout: 10_000 });
  await expect(approvalDialog).toBeHidden({ timeout: 10_000 });
});

test("an option-list permission request renders its own labels", async ({ page, spawnServe }) => {
  // pi's ask_user_question sends one allow_once option per answer (#3741); the fake echoes the chosen id.
  const { serve, sessionId } = await startAcpSession(spawnServe, {
    title: "story-question",
    fakeAcpScript: turn({
      sessionUpdate: "permission_request",
      toolCall: { toolCallId: "pi-ui-1", title: "Pi select", kind: "other", rawInput: { message: "Which option?" } },
      options: OPTION_NAMES.map((name, index) => ({ optionId: `choice-${index}`, name, kind: "allow_once" })),
      echoDecision: true,
    }),
  });
  await openStructuredView(page, serve, sessionId, "ask me something");

  const questionDialog = page.getByRole("alertdialog", { name: /Question/i });
  await expect(questionDialog).toBeVisible({ timeout: 10_000 });
  for (const name of OPTION_NAMES) {
    await expect(questionDialog.getByRole("button", { name })).toBeVisible();
  }
  await expect(questionDialog.getByRole("button", { name: "Allow" })).toHaveCount(0);
  await expect(questionDialog.getByRole("button", { name: "Always" })).toHaveCount(0);

  await questionDialog.getByRole("button", { name: "Option Charlie" }).click();
  // Allow used to silently answer with options[0].
  await expect(page.getByText("permission_option=choice-2")).toBeVisible({ timeout: 10_000 });
  await expect(questionDialog).toBeHidden({ timeout: 10_000 });
});

test("AskUserQuestion card submit resolves and the turn continues", async ({ page, spawnServe }) => {
  const { serve, sessionId } = await startAcpSession(spawnServe, {
    title: "story-ask",
    fakeAcpScript: turn(
      chunk("I need to know your preference..."),
      {
        sessionUpdate: "elicitation_request",
        message: "Which color?",
        requestedSchema: {
          type: "object",
          properties: {
            question_0: {
              type: "string",
              title: "Which color?",
              oneOf: [
                { const: "Red", title: "Red" },
                { const: "Blue", title: "Blue" },
              ],
            },
          },
        },
      },
      chunk("Got your answer."),
    ),
  });
  await openStructuredView(page, serve, sessionId, "help me pick");

  const questionDialog = page.getByRole("alertdialog", { name: /Question from the agent/i });
  await expect(questionDialog).toBeVisible({ timeout: 10_000 });
  // #2145: a pending question is not a stall, so no working spinner.
  await expect(page.getByTestId("acp-working-spinner")).toHaveCount(0);
  const postAnswerChunk = page.getByText("Got your answer.");
  await expect(postAnswerChunk).toHaveCount(0);

  await questionDialog.getByLabel("Blue").check();
  await questionDialog.getByRole("button", { name: "Submit" }).click();
  await expect(postAnswerChunk).toBeVisible({ timeout: 10_000 });
  await expect(questionDialog).toBeHidden({ timeout: 10_000 });

  // #2209: the answer is recorded in the transcript as the user's turn.
  const answerCard = page.getByTestId("elicitation-answer-card");
  await expect(answerCard).toBeVisible({ timeout: 10_000 });
  await expect(answerCard).toContainText("Which color?");
  await expect(answerCard).toContainText("Blue");
});

test("elicitation form with number + boolean fields round-trips", async ({ page, spawnServe }) => {
  const { serve, sessionId } = await startAcpSession(spawnServe, {
    title: "story-mixed",
    fakeAcpScript: turn(
      {
        sessionUpdate: "elicitation_request",
        message: "Configure the run.",
        requestedSchema: {
          type: "object",
          title: "Run options",
          properties: {
            question_0: { type: "integer", title: "Workers", minimum: 1, maximum: 8 },
            question_1: { type: "boolean", title: "Verbose" },
          },
          required: ["question_0"],
        },
      },
      chunk("Configured."),
    ),
  });
  await openStructuredView(page, serve, sessionId, "configure");

  const questionDialog = page.getByRole("alertdialog", { name: /Question from the agent/i });
  await expect(questionDialog).toBeVisible({ timeout: 10_000 });
  const postChunk = page.getByText("Configured.");
  await expect(postChunk).toHaveCount(0);

  await questionDialog.getByPlaceholder("Enter a number").fill("3");
  await questionDialog.getByRole("checkbox").check();
  await questionDialog.getByRole("button", { name: "Submit" }).click();
  await expect(postChunk).toBeVisible({ timeout: 10_000 });
  await expect(questionDialog).toBeHidden({ timeout: 10_000 });
});

test("free-text question (the 'Other' box) submits on Enter, not just the Submit button click", async ({
  page,
  spawnServe,
}) => {
  const { serve, sessionId } = await startAcpSession(spawnServe, {
    title: "story-ask-enter",
    fakeAcpScript: turn(
      chunk("What's your name?"),
      {
        sessionUpdate: "elicitation_request",
        message: "What's your name?",
        requestedSchema: {
          type: "object",
          properties: {
            question_0: { type: "string", title: "Name" },
          },
        },
      },
      chunk("Nice to meet you."),
    ),
  });
  await openStructuredView(page, serve, sessionId, "ask my name");

  const questionDialog = page.getByRole("alertdialog", { name: /Question from the agent/i });
  await expect(questionDialog).toBeVisible({ timeout: 10_000 });
  const postAnswerChunk = page.getByText("Nice to meet you.");
  await expect(postAnswerChunk).toHaveCount(0);

  const answerBox = questionDialog.getByPlaceholder("Type your answer");
  await answerBox.fill("Ada");
  await answerBox.press("Enter");
  await expect(postAnswerChunk).toBeVisible({ timeout: 10_000 });
  await expect(questionDialog).toBeHidden({ timeout: 10_000 });
});
