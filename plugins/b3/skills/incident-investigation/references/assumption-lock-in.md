# Assumption Lock-In: Recognizing and Breaking Free

## The Anti-Pattern

Assumption lock-in occurs when an investigator forms an early mental model of the root cause category (e.g., "this is a scroll problem," "this is a race condition," "this is a memory issue") and then interprets all subsequent evidence through that lens. Failed experiments refine the hypothesis within the same category instead of questioning the category itself.

**Signature:** Three or more failed experiments that all operate within the same conceptual frame. Each failure leads to a more sophisticated version of the same theory rather than a fundamentally different theory.

## Real-World Example: iOS Terminal Black Rectangle

An investigation into a terminal rendering bug on iOS Safari produced this sequence:

| Version | Hypothesis Category | Specific Theory | Result |
|---------|-------------------|-----------------|--------|
| v428 | Scroll | scrollToBottom timing | Failed |
| v429 | Scroll | rAF-deferred scrollToBottom | Failed |
| v434 | Scroll | Synchronous scroll correction | Failed |
| v437 | Scroll | Near-bottom threshold | Failed |
| v438 | Scroll | term.refresh after scroll | Failed |
| v439 | Scroll | vp.scrollTop = scrollHeight | Failed |
| v441 | Scroll | Suppress scroll handler with overflow:hidden | Failed |
| v443 | Scroll | Triple deferred correction (0/0/50ms) | Failed |
| v444 | Scroll | Cursor-aware scroll position | Failed |
| v445 | Scroll | Track manual vs automatic scroll | Failed |

Ten attempts, all within "scroll." The actual root cause was CSS: iOS Safari's scroll-into-view behavior was setting `scrollTop` on a parent `overflow:hidden` container, physically shifting the terminal off-screen. The fix was 4 lines.

**Why lock-in persisted:**
1. Early diagnostic logs showed scroll state (viewportY, baseY) values that looked anomalous
2. Each failed fix revealed new scroll-related data, reinforcing the frame
3. The xterm.js scroll architecture IS complex, providing endless new sub-hypotheses
4. Evidence that contradicted the scroll theory (the empty area "looked like CSS, not terminal") was dismissed because the investigator was already deep in scroll mechanics

## Detection: The Three-Strike Rule

**After three failed experiments within the same hypothesis category, STOP and execute the Category Pivot Protocol.**

This is not optional. Three failures within one category is strong evidence that the category itself is wrong. The probability that the fourth attempt within the same frame will succeed is low. The cost of continuing (more failed deploys, user frustration, context window waste) exceeds the cost of stepping back.

### Counting Strikes

A "strike" is a deployed fix or executed experiment that fails to resolve the symptom. Strikes accumulate within a hypothesis **category**, not individual hypotheses. Categories are broad: "scroll behavior," "timing/race condition," "memory management," "CSS/layout," "network," "configuration."

Refinements within a category count as strikes against the category:
- "scrollToBottom is called too early" -> "scrollToBottom is called but overwritten" -> "scrollToBottom needs to be deferred" = 3 strikes against "scroll behavior"

## The Category Pivot Protocol

When three strikes are reached:

### Step 1: Name the Assumption

Write explicitly in the investigation file: "Current assumption: this is a [CATEGORY] problem." Making the assumption visible is the first step to questioning it.

### Step 2: List What the Assumption Cannot Explain

Review all evidence and identify observations that don't fit the current category. In the terminal bug case:
- The empty area had the exact background color of the CSS, not terminal content
- The bug self-corrected when new terminal data arrived (scroll state was already correct)
- The user described it as "looks like CSS, not terminal"
- All scroll metrics were correct during the bug

These observations were available early but were explained away within the scroll frame.

### Step 3: Generate Hypotheses from OTHER Categories

Force at least 3 hypotheses from fundamentally different categories:

| Category | Hypothesis |
|----------|-----------|
| CSS/Layout | Container element has wrong dimensions or position |
| Rendering | Canvas/DOM renderer is painting at wrong offset |
| Focus/Input | iOS input focus behavior is moving elements |
| Timing | Layout reflow hasn't completed when measurement happens |
| Platform | iOS Safari-specific behavior not present in other browsers |

### Step 4: Design a Discriminating Experiment

Design one experiment that distinguishes between the OLD category and at least one NEW category. The experiment should produce different observable outcomes depending on which category is correct.

Example: "Measure the physical bounding rect of every element in the xterm DOM hierarchy during the bug." This distinguishes scroll (elements positioned correctly but showing wrong content) from CSS/layout (elements physically in the wrong position).

### Step 5: Update Probabilities

Redistribute probability mass. The failed category should drop significantly (its prior was high and it failed three times). The new categories should receive the freed probability.

## Prevention: Hypothesis Diversity Checks

### At Phase 2 (Initial Hypotheses)

Ensure hypotheses span at least 3 different categories. If all 5-8 hypotheses are variations of the same theme, add hypotheses from other categories before proceeding.

**Bad example (all one category):**
- H1: Scroll handler race condition (scroll)
- H2: Scroll position not updated after resize (scroll)
- H3: DOM scrollTop desynced from buffer viewportY (scroll)

**Good example (diverse categories):**
- H1: Scroll handler race condition (scroll)
- H2: CSS container dimensions wrong after keyboard resize (layout)
- H3: iOS Safari scroll-into-view moves parent element (platform)
- H4: xterm canvas render offset miscalculated (rendering)

### At Phase 4 (Revised Hypotheses)

When updating probabilities, check: are all high-probability hypotheses in the same category? If so, explicitly add a hypothesis from a different category and assign it at least 15% probability. This forces investigation of alternatives even when one category seems dominant.

### At Phase 5 (Experimentation)

Before each experiment, ask: "If this experiment fails, will I try another variation of the same approach, or will I pivot to a different category?" If the answer is "same approach," consider whether the current category has already been sufficiently tested.

## The Meta-Lesson

The investigation methodology's strength is structured hypothesis testing. Its weakness is that structure can channel thinking into a groove. The three-strike rule and category pivot protocol add a meta-level check: not just "is this hypothesis correct?" but "is this category of hypothesis correct?"

Repeated failure is information. It's not just "this specific fix didn't work" — it's "the mental model that generated this fix may be wrong." Treat three failures as evidence against the entire frame, not just the specific implementation.
