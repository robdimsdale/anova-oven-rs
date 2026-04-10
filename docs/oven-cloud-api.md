# Anova Precision Oven — Cloud API (Firestore)

User recipes, bookmarks, cook history, and profiles are stored in
**Google Cloud Firestore** under the Firebase project `anova-app`.

The oven app (`com.anovaculinary.anovaoven`) accesses Firestore via the Firebase
Client SDK's native gRPC channel. The REST API at `anovaculinary.io` is a separate,
legacy system for the general Anova Culinary app (sous vide focused) and does **not**
contain oven recipe data.

> Source: Hermes bytecode disassembly of the Anova Oven APK v1.2.8, confirmed by
> running the exact queries via the Node.js Firebase Client SDK.

---

## Authentication

All Firestore access requires a Firebase ID token obtained via `signInWithEmailAndPassword`
(or a refresh token exchange). The Firebase project is `anova-app`.

```javascript
import { initializeApp } from "firebase/app";
import { getAuth, signInWithEmailAndPassword } from "firebase/auth";

const app = initializeApp({
  apiKey: "AIzaSyCGJwHXUhkNBdPkH3OAkjc9-3xMMjvanfU",  // oven app key
  authDomain: "anova-app.firebaseapp.com",
  projectId: "anova-app",
});
const auth = getAuth(app);
const cred = await signInWithEmailAndPassword(auth, email, password);
// cred.user.uid is the Firebase UID used in all queries
```

Both Firebase API keys (general: `AIzaSyB0VNq...`, oven: `AIzaSyCGJ...`) produce
tokens for the same project and the same UID. Either works.

### Firestore Security Rules

The security rules enforce specific query shapes. You cannot list entire collections
or query with arbitrary field filters. Only the exact queries below work. Other queries
(including the Firestore REST API) return `PERMISSION_DENIED`.

The key insight: the `userProfileRef` field is a **Firestore DocumentReference**, not a
plain string. Queries must pass `doc(db, "user-profiles", uid)` as the value.

---

## Collections

### `oven-recipes` (top-level)

All oven recipes (user-created, community, quick-start). Each document is a complete
recipe with metadata and cooking stages.

#### Queries

**User's recipes** (non-draft):
```javascript
query(
  collection(db, "oven-recipes"),
  where("userProfileRef", "==", doc(db, "user-profiles", uid)),
  where("draft", "==", false),
  orderBy("createdTimestamp", "desc"),
  limit(10)
)
```

**User's drafts:**
```javascript
query(
  collection(db, "oven-recipes"),
  where("userProfileRef", "==", doc(db, "user-profiles", uid)),
  where("draft", "==", true),
  orderBy("createdTimestamp", "desc"),
  limit(10)
)
```

**Community/published recipes:**
```javascript
query(
  collection(db, "oven-recipes"),
  where("published", "==", true),
  orderBy("publishedTimestamp", "desc"),
  limit(10)
)
```

**Quick-start recipes:**
```javascript
query(
  collection(db, "oven-recipes"),
  where("isQuickStart", "==", true),
  orderBy("publishedTimestamp", "desc")
)
```

**Single recipe by document ID:**
```javascript
getDoc(doc(db, "oven-recipes", recipeId))
```

#### Document Schema

```typescript
interface OvenRecipe {
  // Identity
  id: string;                          // Same as Firestore document ID
  _id?: string;                        // Alternate ID field
  title: string;
  description: string;
  servings: number;

  // Ownership
  userProfileRef: DocumentReference;   // ref to user-profiles/{uid}
  userId: string;                      // Firebase UID (NOT used in queries)

  // Status
  draft: boolean;                      // true = unsaved draft
  published: boolean;                  // true = visible to community
  isQuickStart?: boolean;              // true = featured quick-start recipe

  // Timestamps (Firestore Timestamps)
  createdTimestamp: Timestamp;
  updatedTimestamp: Timestamp;
  publishedTimestamp?: Timestamp;

  // Content
  steps: Stage[];                      // Cooking stages (same format as WebSocket API)
  ingredients: Ingredient[];
  coverPhotoUrl: string;
  coverVideoUrl: string;
  coverVideoThumbnailUrl: string;
  coverVideoHasAudio: boolean;

  // Metadata
  schema: string;                      // "OvenRecipeV1" or "OvenRecipeV2"
  categories: string[];
  compatibility: object;
  utility: object;
  iconType?: string | null;
  status: string;

  // Stats (server-managed)
  averageRating: number;
  numberOfRatings: number;
  numberOfComments: number;
  numberOfBookmarks: number;
  numberOfCooks: number;

  // Timing
  cookTimeSeconds: number;
  preparationTimeSeconds: number;
}
```

#### Steps Array

The `steps` array contains two types of entries, distinguished by `stepType`:

- **`"stage"`** — oven cook stage. Uses the same format as the WebSocket API's
  `CMD_APO_START` stages. See [WebSocket API — Stage Format](oven-websocket-api.md#stage-format).
- **`"direction"`** — text instruction (not sent to the oven). Contains `title`,
  `description`, and an optional `photoUrl`.

```typescript
// Direction step (text only, not sent to oven)
interface DirectionStep {
  stepType: "direction";
  id: string;
  title: string;
  description: string;
  photoUrl?: string;
}
```

When converting a recipe to `CMD_APO_START`, **filter to `stepType === "stage"` only**:
```javascript
const stages = recipe.steps.filter(s => s.stepType === "stage");
```

```typescript
interface Ingredient {
  id: string;
  description: string;              // e.g., "peeled and shredded potatoes"
  quantity: number;                  // e.g., 6
  unit: string;                     // e.g., "cups", "Tbsp", "tsp", ""
  quantityDisplayType: string;       // "decimal" or "fraction"
}
```

#### Subcollections

| Path | Content |
|------|---------|
| `oven-recipes/{id}/comments/{commentId}` | Recipe comments |
| `oven-recipes/{id}/ratings/{userId}` | Per-user ratings |

### `users/{uid}/favorite-oven-recipes` (subcollection)

User's bookmarked recipes. Each document contains a reference to an `oven-recipes` document.

```javascript
query(
  collection(db, "users", uid, "favorite-oven-recipes"),
  orderBy("addedTimestamp", "desc"),
  limit(10)
)
```

#### Document Schema

```typescript
interface FavoriteOvenRecipe {
  recipeRef: DocumentReference;   // ref to oven-recipes/{recipeId}
  addedTimestamp: Timestamp;
}
```

To get the full recipe, follow the `recipeRef` with `getDoc()`.

### `users/{uid}/oven-cooks` (subcollection)

Saved cook history. Each document represents a completed cook session.

```javascript
query(
  collection(db, "users", uid, "oven-cooks"),
  orderBy("endedTimestamp", "desc"),
  limit(10)
)
```

#### Document Schema

```typescript
interface OvenCook {
  id: string;                        // Same as Firestore document ID
  createdTimestamp: Timestamp;       // When the cook was started
  endedTimestamp?: Timestamp;        // When the cook ended (absent while in-progress)
  recipeRef: DocumentReference;      // ref to oven-recipes/{recipeId}
  stages: Stage[];                   // What was cooked (same stage format)
}
```

**In-progress detection:** documents without `endedTimestamp` represent active cooks.
The `orderBy("endedTimestamp")` query excludes these documents; use
`orderBy("createdTimestamp", "desc")` to find them.

### `users/{uid}` (top-level document)

User preferences.

```typescript
interface UserPreferences {
  preferences: {
    temperatureUnit: "F" | "C";
    ovenLamp: boolean;
  };
}
```

### `user-profiles/{uid}` (top-level document)

Public user profile, referenced by recipes via `userProfileRef`.

```typescript
interface UserProfile {
  name: string;
  bio: string;
  location: string;
  photoUrl: string;
}
```

---

## CRUD Operations

The following operations were confirmed in the Hermes bytecode (function names from
the oven APK string table). They use the same Firebase Client SDK patterns as reads.

| Operation | Bytecode Function | Collection |
|-----------|-------------------|------------|
| Create recipe | `createOvenRecipe` | `oven-recipes` |
| Update recipe | `updateOvenRecipe` / `updateRecipeInFirestore` | `oven-recipes` |
| Delete recipe | `deleteOvenRecipe` | `oven-recipes` |
| Read recipe | `getOvenRecipe` / `fetchRecipeFromFirestore` | `oven-recipes` |
| Watch recipe changes | `updateOnRecipeSnapshot` | `oven-recipes` |
| Watch cook history | `useOnUserCookRecipeSnapshotListener` | `users/{uid}/oven-cooks` |

---

## Legacy REST API (anovaculinary.io)

The REST API at `https://anovaculinary.io` is from the general Anova Culinary app. It
handles sous vide recipes, user identities, and cook history for the sous vide cooker.
**It does not contain oven recipe data.**

The oven app's APK contains dead code referencing `recipesAPIClient.get('/recipes', ...)`
but mitmproxy confirmed the app never calls these endpoints.

### Confirmed Working Endpoints (for reference)

All use `Authorization: Bearer <firebase_id_token>`.

| Method | Path | Returns |
|--------|------|---------|
| GET | `/identities/{uid}` | User identity (email, name, type) |
| GET | `/identities/{uid}/connected-cooks` | Cook history (204 if empty) |
| POST | `/identities/{uid}/connected-cooks/{uuid}` | Save a cook |
| GET | `/v1/users/{uid}` | Public profile |
| GET | `/v1/recipes?page=0&limit=20` | Public sous vide recipes |
| POST | `/v1/recipes` | Create sous vide recipe |
| GET | `/v1/users/{uid}/favorite-recipes` | Sous vide favorites |
| POST | `/authenticate` (header: `Firebase-Token`) | Returns Anova JWT |

### iot-api-prod.anovaculinary.io

AWS API Gateway with minimal endpoints. Not useful for recipes.

| Method | Path | Returns |
|--------|------|---------|
| GET | `/ping` | `"Healthy Connection"` (no auth) |
| POST | `/convert_recipe` | Empty (stub for "Quick Convert" feature) |
| POST | `/ai_assistant` | `null` (stub for AI assistant feature) |

---

## Algolia (Public Recipe Search)

The oven app uses Algolia for searching published community recipes. The index
is `oven_recipes`. Custom/private recipes are NOT in Algolia.

Algolia credentials (from APK — public search-only key):
- App ID: `UH9N6T5UYO`
- API Key: `56d815c8e242f32407f4aed26f0e1627`

---

## Open Questions

1. **`OvenRecipeV1` vs `OvenRecipeV2` schemas** — both exist in the bytecode string
   table. What are the differences? All user recipes retrieved so far have `steps`
   as an array of stages. Does V2 have a different structure (e.g., `cookSettings`)?

2. **Temperature precedence** — stage setpoints include both `fahrenheit` and `celsius`.
   If they disagree, which takes precedence? The `temperatureUnit` user preference
   might determine this, but it's unconfirmed.

3. **Recipe comments and ratings subcollection schemas** — the `oven-recipes/{id}/comments`
   and `oven-recipes/{id}/ratings` subcollections exist but their exact document
   structure hasn't been tested.

4. **Write operations via client SDK** — `createOvenRecipe`, `updateOvenRecipe`, and
   `deleteOvenRecipe` exist in bytecode. Are they accessible with the same
   `signInWithEmailAndPassword` token, or do the security rules restrict writes
   differently than reads?

5. **Pagination** — all queries use `limit(10)` in the bytecode. For users with more
   than 10 recipes, the app likely uses `startAfter()` cursor-based pagination on the
   `createdTimestamp`. The exact mechanism hasn't been confirmed.

6. **`oven-cooks` full document schema** — confirmed: `id`, `createdTimestamp`,
   `endedTimestamp` (absent when in-progress), `recipeRef`, `stages`.

7. **Firestore real-time listeners** — the app uses snapshot listeners
   (`updateOnRecipeSnapshot`, `useOnUserCookRecipeSnapshotListener`). For a CLI that
   wants live updates, could we use `onSnapshot()` through the client SDK?
