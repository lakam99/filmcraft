# Color by feeling

Open the **Color** workspace. **Vibes** is the default; the editor remembers your choice
between Vibes and Advanced in Preferences.

1. Select the video clips that should feel like one scene.
2. Choose **Match selected clips**, or pick a mood to match and style them together.
3. Try **Warm & cozy**, **Cold & tense**, **Nostalgic film**, or **Clean & natural**.
   **More moods** offers Bold & cinematic, Dark & gritty, Bright & airy, and black & white.
4. Adjust **Look strength**, **Brightness**, **Warmth**, and **Color**. Their endpoints say
   what they do: darker/brighter, cooler/warmer, muted/colorful. Fine-tune adds Drama and Texture.

Each clip gets its own automatic balancing correction toward the group's average exposure
and color. The mood is stored once for the whole group. Selecting any member lets you change
that shared look without losing the other members' balancing. No copying grades between clips
is necessary. Existing clip effects remain in place. Deliberately adding a local effect after
the group can give that one clip a different appearance.

**Apply look** compares against the original clip effects by bypassing both the group's
automatic matching and its mood. Setting Look strength to Subtle instead retains the automatic
matching. **Reset adjustments** returns the feeling sliders to their saved starting point.
Choosing a new mood resets these offsets. All grading edits support Undo; a slider drag is
one step. Changing the view to Advanced preserves the exact grade. Returning to Vibes after
an Advanced edit starts from that custom grade.

**Group options** names the group or removes its grading from selected clips. Removing group
grading preserves their original clip effects. Select new clips and use **Join an existing look
group** to add them; all members are rebalanced together. **Match again** recalculates the
balance while keeping the shared mood. Matching samples three frames per clip, runs in a
separate native backend process, and can be cancelled in Progress. A project edit made while
matching is running is preserved; retry matching if the job reports a conflict. Matching
supports up to 256 clips per run.

Group stages currently use CPU grading before GPU composition. Playback speed depends on the
footage, effect settings and playback resolution; the GPU parity checks cover the resulting picture.

Matching is a useful automatic starting point, not scene understanding. Different subjects,
colored lighting, clipped highlights and missing color channels can prevent a close match.
Use separate groups for scenes intended to have different lighting or moods. Skin protection
weights a skin-color range; it is not face recognition. Review the result on your actual shots.

Groups, member corrections, and Vibes settings are saved in the project (schema 18). Older
FilmCraft builds cannot open this schema. Exporting a shared .cube saves only the reusable
creative look and its strength; per-clip balancing stays in the project. The export reports
spatial effects it cannot represent and requires a Rec.709 group without wide gamut.

## Assistant and MCP

Ask for a feeling, for example: “Match these selected shots, make them warm and cozy, but keep
the look subtle.” The built-in assistant and local MCP use the same commands as the panel:

- `lumetri.vibes {}` lists moods and the plain-language adjustment ranges.
- `lumetri.group.create {clips:[...],name:"Kitchen",vibe:"warm-cozy",strength:40}` matches
  selected footage toward its average and creates a persistent group in one edit. Omit `clips`
  to use selected video clips. `replaceGroups:true` explicitly replaces their previous groups.
- Native matching returns `job`, `poll:"jobs.list"`, and `cancel:"jobs.cancel"` immediately.
  Wait for `finished:true`, check `result.error`, then read `backend.output.group`.
- `lumetri.group.setVibe {group:...,warmth:15,brightness:10}` adjusts the shared look.
  Offsets are absolute from a saved base, not increments. Strength is 0–100; offsets are −100–100.
- `lumetri.group.list {group:...,parameters:true}` inspects membership and actual grade values.
- `lumetri.group.setParam`, `.setSection`, and `.setLut` expose the shared Advanced controls.
- `.addClips`, `.balance`, `.removeClips`, `.rename`, `.setEnabled`, `.delete`, and `.exportCube`
  handle the group lifecycle. Rebalancing/addition are background jobs; the other edits are immediate.

Individual color-matching commands still exist for reference-frame workflows in Advanced.
