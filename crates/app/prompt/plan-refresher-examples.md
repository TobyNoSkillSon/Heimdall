## Example 1: a camera sale, waiting on the user's photos (an island without a thread)

Before: state "done", stage "Live", "Live on the marketplace at €450; add real photos later". The conversation since: the listing works, but the user must send real photos to replace the stock ones.

After (its entry in jobs):
```json
{"island": "camera", "title": "Camera sale", "state": "waiting", "stage": "Your photos",
 "summary": "Listed at €450; real photos next, then it goes back live",
 "steps": [
  {"title": "List on marketplace", "state": "done"},
  {"title": "Your real photos", "state": "now"},
  {"title": "Swap them in", "state": "todo"}],
 "progress": {"done": 0, "total": 0, "unit": ""}, "parts": [],
 "facts": [{"label": "Price", "value": "€450"}],
 "image": "", "open": "https://marketplace.example.com/listing/camera",
 "icon": "camera", "clear": []}
```
Why: the next step is the user's, so it is waiting and shows orange, with the camera symbol (an SF Symbol that truly fits). The price is a fact worth a glance; the listing is the open target.

## Example 2: the camera sold (an island without a thread, finished)

Before: state "waiting", stage "Your photos". The conversation since: the user sent the photos, and later said the camera sold for €430 and the buyer collected it.

After (its entry in jobs):
```json
{"island": "camera", "title": "Camera sale", "state": "done", "stage": "Sold",
 "summary": "Sold for €430 and collected",
 "steps": [
  {"title": "List on marketplace", "state": "done"},
  {"title": "Your real photos", "state": "done"},
  {"title": "Sold and collected", "state": "done"}],
 "progress": {"done": 0, "total": 0, "unit": ""}, "parts": [],
 "facts": [{"label": "Sold for", "value": "€430"}],
 "image": "", "open": "", "icon": "", "clear": []}
```
Why: nothing of it runs any more and the user said so, so it is done: the green check. The sale price is the one fact worth a glance; "" keeps the icon and the listing link.

## Example 3: a gated check

Before: "Example app: live checks after the install" (a walk-through after the install).

The pass saw: the install is tonight.

After (the item in scheduled, with its time):
```json
{"id": "p-6b1e0d57", "title": "Example app: checks after install", "done": false, "when": "2026-10-24T23:00",
 "detail": "A walk-through once tonight's install is in.\nSubmenus open left, islands hide and show."}
```

Why: everything gated on tonight's install is one Scheduled item.
