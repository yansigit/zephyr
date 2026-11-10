# Meta Muse: user-facing UI/UX surfaces and controls (as of November 9, 2026)

Source note for the report writer. Two kinds of source carry most of the detail:
1. **Meta primary sources**: the newsroom posts, the "How We Designed Muse" and "How We Built Safety Into Muse" essays, and the Meta Help Center articles. Exact labels from these are quoted below.
2. **An unofficial archive of Muse's own runtime docs**: the GitHub repo `win4r/MuseAI-Skills` (described as "skills and runtime snapshot ... Unofficial archive", created 2026-09-25). It holds files from `/home/hatch/docs/` ("Hatch" is Meta's internal codebase name for Muse, per the safety essay's footnote). These are the instructions the agent reads about its own client UI. They give much finer UI detail than the Help Center, but Meta never published them, they are a September snapshot, and they tell the agent itself: "Never describe approval-card layouts, buttons, or exact label wording ... Presentation varies by app." Everything taken from them is marked **[runtime docs, unofficial]**. Cite with care.

## 1. Apps and screens; the main screen while a task runs

### Takeaway
Muse is its **own app**, not the Meta AI app. It runs on iOS, Android, the web (muse.ai), Mac (since Sep 17) and as a WhatsApp chat. Glasses support is in beta, and Muse Charm voice is announced but not shipped. The home screen is one long, non-turn-based main chat, plus side chats and the tabs Feed, Ideas, Goals and Library. A live status line under the agent's avatar shows what it is doing. Tapping the avatar opens a status panel with Activity, Approvals, Upcoming and Identity. A cloud browser can be opened from the chat to watch the agent ("Open browser"), and the user can "Take control of the browser" (the agent pauses) or "Stop the task".

### Cited Findings
**Surfaces and availability**
- Launch (Sep 8, 2026): "Muse is rolling out in the US on iOS, Android, and muse.ai, and coming soon to AI glasses." Talking to it "works just like messaging another person, in the Muse app or directly in WhatsApp." — [Meta Newsroom](https://about.fb.com/news/2026/09/introducing-muse-personal-ai-agent/)
- Muse "is separate from the existing Meta AI app with its own iOS and Android apps and website". It needs a Meta account, not a Facebook or Instagram account. On the web, login was by phone number. — [Engadget](https://www.engadget.com/2256577/how-to-get-started-with-meta-s-new-ai-agent-muse)
- Login screen: "Log in or create an account / Mobile number or email(required)". — [muse.ai landing](https://applink.muse.ai/)
- The iOS app requires iOS 18 or later and is rated 18+. Muse is for people 18 and over. — [rywalker.com](https://rywalker.com/research/meta-muse); [AP](https://apnews.com/article/meta-muse-ai-agent-3a4572eb4cf4e95d8a0dfdad6e6ca065)
- Muse for Mac arrived on Sep 17: "your agent can now get stuff done right on your computer — files, messages, calendar, notes". — [TechCrunch](https://techcrunch.com/2026/09/18/metas-muse-hits-mac-letting-the-ai-take-actions-on-your-computer/)
- At Connect (Sep 23) Meta listed today's surfaces as "the Muse app, WhatsApp, the web, and the Mac desktop version". It added computer use on Mac ("With your permission, Muse can now drive any app on your Mac"), announced glasses "in the coming months", and showed the Muse Charm device and a Muse email address. — [Meta Connect 2026 recap](https://www.meta.com/blog/meta-connect-2026-everything-we-announced/)
- Glasses: "How to use Muse on your AI glasses (Beta)". It needs both the Meta AI app (pairing and settings) and the Muse app (conversations). To switch, go to Glasses > Device settings > AI assistant > "Muse or Meta AI". "Only one assistant is active on your glasses at a time." The wake words are "Hey Meta" or "Hey Muse" or a custom name, though the custom name is not on all models during the beta. — [Meta Help: glasses](https://www.meta.com/help/ai-glasses/1391796469223757/)
- Regions: US at launch. Singapore was added on Oct 27. — [Meta Newsroom](https://about.fb.com/news/2026/10/muse-is-now-available-in-singapore-the-worlds-first-personal-ai-agent-built-for-everyone/) The runtime docs say "available in the US and Canada" **[runtime docs, unofficial]** — [client-surfaces.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/client-surfaces.md). WhatsApp Help says Muse is "currently in testing and only available in limited countries". — [WhatsApp Help](https://faq.whatsapp.com/1131618509041969)
- WhatsApp is a **messaging channel**, not a connector. You connect it under Settings > **Messaging Channels** > WhatsApp > link, or by telling Muse in chat ("Connect WhatsApp > Connect"). It can only be set up from your primary phone. "your Muse agent can only see your conversation with it on WhatsApp." After you disconnect, old messages stay viewable, but you can no longer send new ones from WhatsApp. — [WhatsApp Help: connect/disconnect](https://faq.whatsapp.com/1516007700293140?cms_platform=android)
- No source names Messenger or Instagram as a chat surface for Muse. Instagram, Facebook and Threads are connectors (see section 4). Messenger was reportedly named as a connector in Alexandr Wang's launch thread, which is secondary reporting. — [CellCog](https://cellcog.ai/blog/muse-connector-platform)

**Main screen structure**
- Design: "one long conversation in a main chat ... Muse isn't a turn-by-turn experience. You can interrupt it or send multiple tasks at once". Side chats were added for separate context. Chat bubbles are used because proactive messages arrive out of turn. — [How We Designed Muse](https://introducing.muse.ai/)
- "Open Muse, which will begin on the Chat tab ... You don't have to wait for Muse to respond before asking your next question." — [Meta Help: get started](https://www.meta.com/help/artificial-intelligence/1331373868832401)
- The Help Center names the tabs **Chats**, **Goals** and **Library** (with **System Files** inside Library), and the **Assistant icon** opens **Upcoming** and **Identity** (Memory, Soul). — [Meta Help: manage data](https://www.meta.com/help/artificial-intelligence/2225571704857152/) The **Ideas** tab: "as tips in your first days of onboarding; as suggestions in the Ideas tabs". — [How We Designed Muse](https://introducing.muse.ai/)
- Every surface has the tabs **Chat, Feed, Ideas, Goals, Library**. The web adds a left icon rail with **Search** (Cmd+K). Feed holds personal "editions" the agent writes on a schedule from a feed prompt, which the user edits (on the web: "Edit" and "Generate" buttons). Library has sections for Artifacts (all, documents, web artifacts), Media and System files. Each card's menu offers Pin, Share, Download and Delete. **[runtime docs, unofficial]** — [client-surfaces.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/client-surfaces.md)
- Nov 6: **Spaces**, custom mini-apps that Muse builds from a description. You can "add them to your sidebar, move them into your main tabs, hide the ones you don't need right now, and schedule when they appear". They are "available everywhere Muse is available". — [Meta Newsroom: Spaces](https://about.fb.com/news/2026/11/introducing-spaces-customize-muse-app/)

**While a task runs**
- "Your assistant avatar at the top of your screen shows live status updates (examples: "Is working," "Making something", "Is updating memory")." — [Meta Help: guidance and approval](https://www.meta.com/help/artificial-intelligence/1385290430137537/)
- "under the Avatar, you'll see a snippet of what it's working on. Tapping on your Muse avatar will show you its full activity log as well as permissions you've approved." — [How We Designed Muse](https://introducing.muse.ai/)
- Status screen tabs: **Activity, Approvals, Upcoming, Identity**. On Android it opens on Approvals when one is waiting. On iOS and web it opens on Activity, and opens on Approvals when the user arrives from an approval prompt. "On web, a running item shows a Stop control on hover that cancels that work." On the web the status screen opens as a right-side panel from the avatar at the top centre of the chat column. **[runtime docs, unofficial]** — [client-surfaces.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/client-surfaces.md)
- Background work: "When it completes its background work, it evaluates whether the result is worth surfacing, and notifies you only when something is meaningfully new or needs your input." — [How We Designed Muse](https://introducing.muse.ai/)
- Live browser: "Tap **Open browser** in the chat, which opens a viewing window ... Select **Take control of the browser** to pause what your Muse is doing in the browser. Select **Stop the task** to terminate". — [Meta Help: browsing](https://www.meta.com/help/artificial-intelligence/2124746764949121)
- "Users can see what Muse is doing in the browser and take over at any time ... When the user takes over control of the browser, or while secure credential storage is filling a form, the agent is paused and can't act at all." — [How We Built Safety Into Muse](https://research.meta.ai/blog/security-and-safety-for-ai-agents-our-approach-with-muse)
- Mobile has "touch control when taking over the live VM browser". Live viewing and takeover "varies by client surface". **[runtime docs, unofficial]** — [client-surfaces.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/client-surfaces.md); [browser.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/browser.md)
- "Undo or stop: You can ask your Muse to undo or stop some actions, like reading a website or comparing prices for a purchase, in chat. Some other actions, like sending an email, cannot be reversed". — [Meta Help: guidance and approval](https://www.meta.com/help/artificial-intelligence/1385290430137537/)

**Mac app extras [runtime docs, unofficial]** — [client-surfaces.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/client-surfaces.md)
- The Mac app wraps the web app. It adds a menu bar icon (Open Muse, Quick chat, Voice dictation, Check for updates…, Quit). **Quick chat** opens with Option-Space as a card over any app. A floating button stays on screen after the window closes. Holding fn dictates anywhere. Settings > **File system access** sets Mail.app, Messages.app, Notes.app and WhatsApp.app to "Read only, Read and interact, or Off", with a "Blocked folders" list. "The Mac app gets no push notifications."

### Inferences
- The pattern for a gateway: one persistent main thread, optional topic threads, a one-line live status under an identity avatar, and a status drawer (Activity / Approvals / Upcoming / Identity) one tap away. Each of these maps to an existing Zephyr concept: the `main` session, the chat, `/held` approvals, the scheduler and the memory files.
- "Take control" pauses the agent rather than racing it. That is a deliberate mutual exclusion worth copying for any shared browser or desktop view.

### Gaps
- No source describes a progress bar or step list for a running task beyond the status snippet and the Activity log.
- No source says whether WhatsApp users can open the live browser. The runtime docs say app navigation "cannot [be tapped] there", so WhatsApp users are pointed back to the app.
- No source covers Muse on Messenger or Instagram as chat surfaces. None was found.

## 2. Approval cards: triggers, options, delivery

### Takeaway
Approvals are deterministic UI cards. Sentinel, a separate permission agent, raises them outside the conversation, and they are not chat messages. They fire for connector writes and "important reads", for sharing your data or visiting unfamiliar sites (under the default "Ask for some actions"), for checkout pages, and for every purchase. The documented options are **Allow once / Allow for this task / Allow for this site / Always allow / Deny**, plus a **See task details** link. Pushes carry Allow/Deny buttons. Spending can never be pre-approved.

### Cited Findings
- "When Muse takes an action, it will ask you for approval in line with the permissions settings you've set up. Approval options for actions can include:
  - **Allow once**: Muse proceeds this one time
  - **Allow for this task**: Muse can take this type of action for the entire task
  - **Allow for this site**: Muse can take this type of action for this website in the future without asking again
  - **Always allow**: Muse can take this type of action for this Connector in the future without asking again
  - **Deny**: Muse won't proceed this one time
  You can tap **See task details** to see additional details about the task before deciding what permission to grant." — [Meta Help: guidance and approval](https://www.meta.com/help/artificial-intelligence/1385290430137537/)
- Note: Meta documents no "Always ask" option on the card. "Always ask" is a **default setting** in Permissions (see section 3). "Deny" is per-occurrence ("this one time"). No "always deny" option on the card is documented.
- Sentinel decides "allowed, denied, or to ask". When it asks, "Sentinel creates a pending approval and execution stops ... A dialog is presented to the user directly within the client UI — not via their conversation with Muse". Grants are "strict capabilities, not conversational suggestions", "bound to the particular connector/destination and use case". Muse supports "one-time, session-scoped, task-scoped, time-bounded, or perpetual permission. Sentinel decides which grant types to present". Sentinel generates "a user-visible purpose for the request". — [How We Built Safety Into Muse](https://research.meta.ai/blog/security-and-safety-for-ai-agents-our-approach-with-muse)
- Signal-to-noise: "Read-only, previously allowed, or demonstrably low-risk actions can proceed without interruption". Taint tracking means that when a process has read user data, it "lose[s] auto-allow and fall[s] back to the normal approval flow". — [How We Built Safety Into Muse](https://research.meta.ai/blog/security-and-safety-for-ai-agents-our-approach-with-muse)
- Design intent: "structured approval cards with clear accept/reject actions ... we also wanted to avoid banner blindness ... the default allows for standard browsing of the web and stops for anything hard to undo." — [How We Designed Muse](https://introducing.muse.ai/)
- Browser classifiers "either block the action or prompt the user to review". They watch for egress of unrelated personal data, prompt injection in the DOM, images or downloads, and "Attempted submission of high risk forms". — [How We Built Safety Into Muse](https://research.meta.ai/blog/security-and-safety-for-ai-agents-our-approach-with-muse)
- Triggers named by Meta: sending email and making a purchase (launch post). For Muse for Business (Sep 29), Muse "will not post content, send messages or buy anything without the owner's approval". — [Meta Newsroom](https://about.fb.com/news/2026/09/introducing-muse-personal-ai-agent/); [The Next Web](https://thenextweb.com/news/meta-muse-small-business-ai-agent)
- Delivery: "approval request can reach them as a push notification. On Android and iOS the notification shows Allow and Deny buttons directly in the notification shade ... On iOS, acting on them requires unlocking and foregrounding the app; ... on Android swiping the notification away counts as Deny". **[runtime docs, unofficial]** — [client-surfaces.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/client-surfaces.md)
- "an approval sends a push the moment it happens. An approval raised in the user's linked side chat also appears there ... There is no quiet-hours setting, and the user has no control over how approvals are routed ... Approvals cannot be deferred". **[runtime docs, unofficial]** — [calls-texts-notifications.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/calls-texts-notifications.md)
- A third-party setup guide says the request "appears in the approval section on the right of the app" (web). — [AgentMail](https://www.agentmail.to/build/muse)
- Email sends from chat are "approved each time too, except a Gmail send, or a Mail send from the Mac app, to a recipient the user has already allowed". **[runtime docs, unofficial]** — [calls-texts-notifications.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/calls-texts-notifications.md)
- CAPTCHAs: the agent asks once whether it may solve them, scoped "just this time, this site only, ask each time, or never". **[runtime docs, unofficial]** — [browser.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/browser.md)
- In passing: a user reported that Muse gave his home address to a Facebook Marketplace buyer. The incident is outside this note's scope. — [The Next Web](https://thenextweb.com/news/meta-muse-facebook-marketplace-address-buyer-robb)

### Inferences
- The card's scope ladder is once, task, site (for web), connector (for "Always allow"). These are four grant scopes plus a one-shot deny. That is close to Zephyr's `/ok N [once]` and timed passes. Muse's "for this task" scope has no direct Zephyr equivalent.
- The card is answered out of band (Sentinel to client and back), never by a chat "yes". That matches Zephyr's rule that grants come only from the owner, through a deterministic path.

### Gaps
- No primary source gives the card's visual layout (fields shown, amounts, recipient) beyond "the exact action to be approved".
- Lock-screen behaviour is documented only in the unofficial runtime docs.
- No source documents a time-bounded grant option label ("for 1 hour"), although the safety essay says the capability exists.

## 3. Activity log, permissions screens and revocation

### Takeaway
The Activity log opens from the avatar. It is a chronological record of actions taken and permissions given, with step-level tool-call lineage per reviewers. Grants are revoked in **Settings > Permissions**, not from the Activity log itself. That screen holds defaults for Connectors and Web access ("Ask for some actions" / "Always ask") and **Manage permissions** (per Connector, Artifacts and scheduled tasks, Allowed websites, with revoke).

### Cited Findings
- "Visit your **Activity log** by tapping your assistant icon to see a chronological record of actions your Muse has taken and permissions you've given it ... Browse your **System Files** for a deeper look into your Muse's activities." — [Meta Help: guidance and approval](https://www.meta.com/help/artificial-intelligence/1385290430137537/)
- "Muse shows people a complete audit trail of everything it has done and plans to do." — [Meta Newsroom](https://about.fb.com/news/2026/09/introducing-muse-personal-ai-agent/)
- Reviewer: "this activity feed. So it will actually trace for you every task that it's done for you over time ... this lineage and this history". Her episode chapter is "Activity feed design deep dive: tool calls and step-by-step lineage". — [How I AI transcript](https://ulisten.ai/channels/how-i-ai/muse-gets-ai-agent-ux-right_2UwemqPkJSQ/transcript); [Apple Podcasts](https://podcasts.apple.com/us/podcast/muse-review-the-personal-ai-agent-that-gets-consumer/id1809663079?i=1000790016753)
- "To manage your Muse permissions, navigate to **Settings**, then tap **Permissions**. Here you can manage:
  1. **Connectors**: **Ask for some actions** asks "before every write action and important read actions"; **Always ask** asks "before any action."
  2. **Web access defaults**: **Ask for some actions** asks "when your information may be shared or it visits a website that is unfamiliar"; **Always ask** asks "before accessing any website."
  3. **Manage permissions**: **Connectors** (per Connector), **Artifacts and scheduled tasks** ("manage your approvals for each scheduled task and artifacts"), **Allowed websites** ("view which websites you've allowed ... and revoke permission")." — [Meta Help: guidance and approval](https://www.meta.com/help/artificial-intelligence/1385290430137537/)
- Default: Meta's default is "ask for some actions". — [Engadget](https://www.engadget.com/2256577/how-to-get-started-with-meta-s-new-ai-agent-muse)
- Web Permissions pane detail **[runtime docs, unofficial]** — [client-surfaces.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/client-surfaces.md):
  - The overview shows the Connector and Web-access defaults, an **Advanced network settings** card and **"Reset approvals to defaults"**.
  - Manage permissions rows: Connectors, Websites ("Websites you've allowed", each with revoke), Artifacts, Scheduled tasks (per-connector **Allow / Ask / Deny**, with a Revoke on each allowed website) and **Direct network protocols**.
  - Direct network protocols covers SSH, sending email, mailbox access, databases, FTP, external DNS and other TCP/UDP. Each is a "switch between Deny and Ask", every row starts on Deny, and "there is no Allow choice".
  - Advanced network settings has three switches: Transparent proxy, TLS interception, SNI mismatch rejection.
  - Connector detail pages show **"Manage recipient permissions"** with a **Revoke** per allowed recipient (Gmail send, Calendar invite, Drive share).
  - When a scheduled task is created, the chat shows "Scheduled task <title> was created." with a View action.
- The Android Permissions row is described as "default permissions for connectors, active permissions for individual tasks". **[runtime docs, unofficial]** — [client-surfaces.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/client-surfaces.md)
- The user can also ask in chat: "Show all URLs you have visited in the last week" or "Show me the cookies you've accepted." — [Meta Help: browsing](https://www.meta.com/help/artificial-intelligence/2124746764949121)

### Inferences
- Muse separates the log you read (Activity) from the place you revoke (Settings > Permissions > Manage permissions). Each grant is listed under the object it scopes: a connector, a website, a scheduled task, an artifact, a recipient.
- "Reset approvals to defaults" is a one-button panic control that is cheap to copy.

### Gaps
- No source confirms that a grant can be revoked from an Activity log entry itself. The documented revoke paths are all under Settings > Permissions and connector detail pages.

## 4. Connectors, read/write permissions, web access

### Takeaway
Connectors are added under Settings > Connectors ("Connect" > "Continue" > provider login) or by asking in chat. Each has its own permission tree, and many can be set to read-only. Facebook, Instagram and Threads connect automatically through Accounts Center. Custom connectors are built by Muse and not reviewed by Meta. The Browser counts as an always-on connector with per-method Allow/Ask/Deny settings.

### Cited Findings
- "You can connect a Connector by asking Muse (example: "Connect my Gmail"), or ... Go to **Settings**, then tap **Connectors**. Tap **Connect** next to the Connector ... Tap **Continue** to authorize". Muse exchanges only task-relevant data and won't "download your whole inbox (unless you tell it to)". Some connectors push changes proactively, and Meta flags these "before you connect them". — [Meta Help: Connectors](https://www.meta.com/help/artificial-intelligence/1687253048996149/)
- "By default, Muse will not take many important actions, like sending an email, without your approval. Many Connectors can also be set up so that Muse is only able to retrieve data, but not take actions". For email, "people choose ... whether it reads their mail or can also send on their behalf." — [Meta Help: Connectors](https://www.meta.com/help/artificial-intelligence/1687253048996149/); [Meta Newsroom](https://about.fb.com/news/2026/09/introducing-muse-personal-ai-agent/)
- Disconnecting "stops Muse from exchanging data", but earlier information "might still remain in Muse's memories". — [Meta Help: Connectors](https://www.meta.com/help/artificial-intelligence/1687253048996149/)
- "Facebook, Instagram and Threads – are connected automatically if you have your accounts in the same Accounts Center". Custom connectors are stored in the "Secure Credentials Store", and "Meta doesn't review custom connectors". Apple Health and Android SMS access is managed in device settings. — [Meta Help: Connectors](https://www.meta.com/help/artificial-intelligence/1687253048996149/)
- Named at launch (per Wang's thread, secondary): Gmail, Google Calendar, Outlook, Plaid, OpenTable, Google Docs, Spotify, Function Health, Withings, Tailscale, Peloton, plus Instagram, Threads, Facebook and Messenger. — [CellCog](https://cellcog.ai/blog/muse-connector-platform)
- Connect added Walmart, Best Buy, American Eagle Outfitters, DICK'S Sporting Goods, … GitHub and Box (the list is truncated in the source), plus "access to the entire Shopify catalogue". — [Meta Connect recap](https://www.meta.com/blog/meta-connect-2026-everything-we-announced/)
- Small business (Sep 29): Shopify, QuickBooks, Stripe and Canva. — [Reuters](https://www.reuters.com/business/media-telecom/meta-expands-muse-ai-agent-small-businesses-2026-09-29)
- Email connector "filters out one-time tokens, password reset links, and login magic links". — [How We Built Safety Into Muse](https://research.meta.ai/blog/security-and-safety-for-ai-agents-our-approach-with-muse)
- Connector detail **[runtime docs, unofficial]** — [client-surfaces.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/client-surfaces.md):
  - Multi-account connectors show a "Default" badge, "Add account" and per-account disconnect.
  - A scope the user has not yet granted shows "Needs additional access" with an Add control.
  - The **Browser connector is always on** with no disconnect. Its permissions are "visit websites, submit a form or POST request, download files, upload files, and fill saved credentials, each settable to Allow/Ask/Deny per method, with a reset to defaults".
- Mac: macOS permissions **Full Disk Access**, **Automation** and **Notifications**. Muse Settings > **File System Access** turns off app access. "When your agent deletes a file, it moves it to your Trash". — [Meta Help: Mac](https://www.meta.com/help/artificial-intelligence/1126304576638594)

### Inferences
- Per-connector Allow/Ask/Deny by method, with defaults set globally, is the shape to copy. A gateway equivalent would be one permission tree per MCP server or tool, with per-recipient standing allows listed beside it and revocable.

### Gaps
- Meta has published no full connector list. No source gives the exact per-connector permission labels beyond the browser's.

## 5. The Secure VM / cloud computer

### Takeaway
Each user gets a persistent, isolated Linux VM ("Muse Secure VM") with a full Chromium browser, files and a terminal. The user can watch the browser and take it over. The VM's file system shows up as **System Files** in Library. Confidential VM (user-held key) was announced for "later this year". The runtime docs show it existing in some accounts, but its public rollout is unconfirmed.

### Cited Findings
- "Muse Secure VM is a persistent, isolated Linux computer with a full browser that you and your agent can use to complete tasks, allowing you to intervene at any point." — [muse.ai](https://applink.muse.ai/)
- "Muse has its own computer, with a file system and a terminal, so it can write its own code and build the tools a task needs." — [How We Designed Muse](https://introducing.muse.ai/)
- "real up-to-date Chromium based browser running behind a virtualization layer". Sign-in uses "a custom UI on the client to capture your username and password", which routes straight to `authd`. The agent cannot run JS in the page, and DevTools are disabled. — [How We Built Safety Into Muse](https://research.meta.ai/blog/security-and-safety-for-ai-agents-our-approach-with-muse)
- "You have access to a web browser inside your chat with Muse, which both you and Muse can control." — [Meta Help: browsing](https://www.meta.com/help/artificial-intelligence/2124746764949121)
- Muse runs in a Debian `systemd-nspawn` container, with Sentinel and credential storage outside it. — [rywalker.com](https://rywalker.com/research/meta-muse)
- Confidential VM: "Later this year, Meta will introduce Muse Confidential VM ... encrypted with a key only they hold". — [Meta Newsroom](https://about.fb.com/news/2026/09/introducing-muse-personal-ai-agent/) The runtime docs refer to confidential-VM sessions with an **Encryption** settings pane and a recovery PIN, and say several features are absent on them (no Messaging Channels, no training toggle, no subscription area). **[runtime docs, unofficial]** — [client-surfaces.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/client-surfaces.md)

### Inferences
- The VM's file tree is exposed as a read-only "System Files" view, and the browser is a watchable, take-over-able pane in the chat. That fits Zephyr's container plus Xvfb desktop, with the owner given a view and a pause-and-take-over control.

### Gaps
- Whether Confidential VM was generally released by November 2026 is unconfirmed. No public announcement was found.
- Only the browser is documented as viewable and take-over-able. No source shows a full desktop or terminal view for the user.

## 6. Payments

### Takeaway
Payments go through a **Wallet** (Stripe Link at launch, Shop Pay "coming soon" at launch). Each purchase creates a single-use virtual card tied to the merchant, the amount and a short time window. Every purchase needs an approval card with the exact details, and spending cannot be pre-granted. There are no user-set budgets. The only standing caps are Link's own per-purchase, daily and 30-day limits.

### Cited Findings
- "Link's wallet for agents generates a one-time-use card so your real card details stay hidden ... Shop Pay is coming soon as another way to pay, along with 1Password support". — [Meta Newsroom](https://about.fb.com/news/2026/09/introducing-muse-personal-ai-agent/)
- On sites where your card is already on file, "detection that you're on a checkout page ... prompt[s] for a human in the loop approval with the exact details of the purchase every time". The card is "tied to that particular merchant, a particular dollar amount, and only valid for a limited period of time". — [How We Built Safety Into Muse](https://research.meta.ai/blog/security-and-safety-for-ai-agents-our-approach-with-muse)
- "Muse checkout approval controls" are shown as an example of deterministic UI. — [How We Designed Muse](https://introducing.muse.ai/)
- Stripe says you approve the transaction total in the chat. — [shipwithmuse.live (citing Stripe)](https://shipwithmuse.live/blog/muse-whatsapp)
- Wallet behaviour **[runtime docs, unofficial]** — [payments-and-purchases.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/payments-and-purchases.md):
  - "Every purchase re-prompts" and the approval is bound to merchant, amount and payment selection. The user can pick a different saved Link card on the card itself.
  - A checkout consent, once given, covers the same task and merchant for about 3 hours.
  - A pending checkout expires after about 10 minutes.
  - "Approvals cannot be pre-granted, batched, or automated"; a background task "can get as far as a pending approval card".
  - "You cannot set up budgets or allowances". Link has per-purchase, daily and 30-day limits. The virtual card is funded up to 5 units above the total.
  - No peer-to-peer payments.
  - Settings > **Wallet**: provider connection with Add, Disconnect and "one write-permission choice for creating spend requests".
  - Merchant-saved-card checkouts need chat confirmation of the exact terms.
  - The runtime docs treat Shop Pay as supported by the Sep 25 snapshot. No public Meta announcement of that ship date was found.

### Inferences
- Zephyr's payment gate (hold, card, `/ok`) matches Muse's "every purchase re-prompts" model. Muse adds amount-bound one-time credentials and short expiry on pending checkouts.

### Gaps
- No Meta-published spending-limit UI. The limits belong to Link and are not settable in Muse.

## 7. Phone calls and voice

### Takeaway
Muse places **outbound calls to US businesses** in a gradual beta (from about Sep 16). An AI caller with a choice of voice handles the call, and the user confirms it first. It cannot receive calls or text from its own number. Real-time voice chat with an expressive avatar rolled out on Nov 5 (US) on mobile, web and Mac via a phone icon. Voice on glasses and Muse Charm is "coming soon".

### Cited Findings
- "Meta's Muse ... just gained the ability to place calls to U.S. businesses ... will first roll out to users who ask Muse to make a call." Wang called it a "beta for outbound calls to US businesses". — [TechCrunch](https://techcrunch.com/2026/09/17/rival-ai-agents-instinct-and-metas-muse-both-add-the-ability-to-make-calls/)
- "We're also experimenting with the ability for Muse to make phone calls on your behalf to businesses whose phone numbers are publicly available". Businesses can opt out by form. — [Meta Help: non-users](https://www.meta.com/help/artificial-intelligence/4532990443643263)
- Call mechanics **[runtime docs, unofficial]** — [calls-texts-notifications.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/calls-texts-notifications.md):
  - Calls happen "now or on a schedule, with the user's explicit confirmation". The user picks a human or AI caller where human calling is offered. The AI voices are "Hailey" (female) and "Brett" (male).
  - "Phone cannot receive calls. Phone cannot send texts." Cold calling and bulk calls are refused.
  - On Android with pairing, Muse can dial from the user's own number. Texting needs a paired Mac (iMessage) or Android set as default assistant. On iPhone it can only draft texts.
- Voice: "just tap the phone icon in the top-right corner to start talking". Presence modes are "voice only, the avatar alongside your conversation, or full screen". There are "hundreds of voices". Rolled out in the US first. "Voice conversations ... coming soon to AI Glasses and Muse Charm." — [Meta Newsroom: voice](https://about.fb.com/news/2026/11/introducing-muse-ai-voice-expressive-avatars/)
- Voice mode keeps running in the background with the microphone indicator on. Say "Goodbye" or "Hang up" to end. — [Meta Help: get started](https://www.meta.com/help/superintelligence/1331373868832401/)
- A voice call with the agent's character appears in marketing art. — [muse.ai](https://applink.muse.ai/)

### Gaps
- No Meta primary source gives the call-confirmation UI or the transcript UI.

## 8. Routines, goals, scheduled tasks, check-ins

### Takeaway
Reminders and recurring tasks are created in chat and listed under **Assistant icon > Upcoming**. Edits on iOS and web draft a chat message rather than opening a form. The **Goals** tab holds goals with plans, progress entries, check-ins and briefings. Proactive messages are tuned by telling Muse ("turn it off, dial it down, or dial it up"). The **Feed** tab holds scheduled personal editions.

### Cited Findings
- "Set one-time or location-based reminders ... Create recurring tasks: Schedule something to repeat daily, weekly, or on a custom interval", such as "daily check-ins or weekly summaries". You can view them by asking or via "Assistant Icon ... Upcoming tab". "Reminders are delivered as messages in your conversation". "Recurring tasks continue until you cancel them." — [Meta Help: reminders](https://www.meta.com/help/artificial-intelligence/1484325780075655)
- Goals: "Personal goals that you have set, which Muse will help you track progress and send check-ins. Navigate to the Goals tab ... three dot menu, select Delete". — [Meta Help: manage data](https://www.meta.com/help/artificial-intelligence/2225571704857152/)
- "The Goals tab is where Muse keeps track of all this for you, and you can interact directly there or talk to it about these tasks in chat." — [How We Designed Muse](https://introducing.muse.ai/)
- Proactivity: "you can always tell your Muse to turn it off, dial it down, or dial it up." — [How We Designed Muse](https://introducing.muse.ai/)
- Goal controls: Complete/un-complete, Rename, Delete (with confirmation). Subgoals go one level deep, and "Add Subgoal ... starts a chat message". There is no pause state. "Goal briefings" are letters on a system schedule. **[runtime docs, unofficial]** — [goals.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/goals.md)
- Upcoming: a task can be removed from its detail view, and Android and web show run history. Edit "drafts a chat message". **[runtime docs, unofficial]** — [client-surfaces.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/client-surfaces.md)
- Proactive pushes come "at most once a day" for ordinary items, during waking hours. Urgent ones come at once. Preferences are kept in a file the agent edits. "There is no quiet-hours setting." **[runtime docs, unofficial]** — [calls-texts-notifications.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/calls-texts-notifications.md)
- A user: "Muse monitors the schedules and sends me a daily push notification lining out everyone's day". — [messagingagents.com (quoting a user post)](https://messagingagents.com/agents/muse)

### Gaps
- No documented per-goal check-in frequency setting.

## 9. Memory, forgetting, persona

### Takeaway
Memory is plain files the user can read and edit: **Memory.md, Soul.md (personality, boundaries) and Identity.md (name, creature, vibe, tagline)**, under Assistant icon > Identity. The **forget skill** is invoked in chat. **Reset Muse** in Data controls wipes everything. Name, avatar, voice and tone are all customisable.

### Cited Findings
- "**Memories** ... Tap the Assistant icon, then **Identity**, then **Memory**. You can edit and delete information in the Memory.md file directly." "**Soul**: Who your Muse is, including core truths, boundaries, and personality ... Soul.md". "**Identity**: ... its name, creature, vibe, or tag line ... Identity.md ... **Edit**." — [Meta Help: manage data](https://www.meta.com/help/artificial-intelligence/2225571704857152/)
- "**Forgetting**: ... use the forget skill: simply ask Muse to forget a topic or set of interactions ... remove it to the best of its ability." "After you delete something from Muse, Muse may still remember information it learned from what you deleted." — [Meta Help: manage data](https://www.meta.com/help/artificial-intelligence/2225571704857152/)
- Deletes: long-press a message > **Delete**. Side chat three-dot menu > **Delete**. Library file delete. **Settings > Data controls > Reset Muse > Reset** ("cannot be undone"). **Download your Muse data**. Toggle **Help improve our AI models** > **Turn off** (it is on by default and the change applies to past interactions too). — [Meta Help: manage data](https://www.meta.com/help/artificial-intelligence/2225571704857152/)
- Customise "Name ... Personality and tone ... Communication style ... Memories ... Emoji and identity" by telling Muse. **Import memory**: Settings > Data Controls > Import memory > upload a .zip exported from another assistant. — [Meta Help: personality](https://www.meta.com/help/artificial-intelligence/995796179982326/)
- Avatar: users name it and restyle it by prompt ("Change your avatar to…"). It refused the name "Mark Zuckerberg". — [How I AI transcript](https://ulisten.ai/channels/how-i-ai/muse-gets-ai-agent-ux-right_2UwemqPkJSQ/transcript); [WSJ](https://www.wsj.com/tech/personal-tech/meta-muse-ai-agent-review-ab956101)
- Web Identity tab opens MEMORY, SOUL and IDENTITY "in a dialog. Memory is that editable file, not a list of rows." **[runtime docs, unofficial]** — [client-surfaces.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/client-surfaces.md)

### Inferences
- Exposing memory as editable markdown files, plus a chat-driven "forget", is cheap to copy and fits Zephyr's git-committed memory files.

### Gaps
- Meta documents no one-tap "forget this" button on a message. Forget is conversational only.

## 10. Onboarding

### Takeaway
Sign in with a Meta account (phone or email). Muse explains what it can do, then asks you to **name it and pick an avatar**. Ideas and tips appear in the first days. Defaults: training **on**, permissions **"Ask for some actions"**, Facebook/Instagram/Threads auto-connected through Accounts Center, proactivity on. Subscription offers are shown during onboarding.

### Cited Findings
- "After logging in, one of the first things Muse will ask is for you to rename it ... You can also change its avatar." The reviewer recommends turning off "help improve our AI models" and checking Permissions (default "ask for some actions"). — [Engadget](https://www.engadget.com/2256577/how-to-get-started-with-meta-s-new-ai-agent-muse)
- "it tells you upfront what Muse can do for you and why it's great for you." — [How I AI transcript](https://ulisten.ai/channels/how-i-ai/muse-gets-ai-agent-ux-right_2UwemqPkJSQ/transcript)
- Ideas come "as tips in your first days of onboarding". — [How We Designed Muse](https://introducing.muse.ai/)
- "Available benefits will be shown to you during onboarding before you subscribe." The plan is free with a usage limit. Reviewers report Power at $20/month and Maximum at $100/month, with weekly Muse-token allowances. — [Meta Help: subscriptions](https://www.meta.com/help/subscriptions/1625680452306909/); [rywalker.com](https://rywalker.com/research/meta-muse)
- A new user's Feed opens with intro posts ("Getting started"). An invite code can be redeemed during the first 48 hours. **[runtime docs, unofficial]** — [client-surfaces.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/client-surfaces.md)
- Mac setup asks for macOS permissions (Full Disk Access, Automation, Notifications, plus Microphone and Accessibility for dictation). — [Meta Help: Mac](https://www.meta.com/help/artificial-intelligence/1126304576638594); [client-surfaces.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/client-surfaces.md)

### Gaps
- No step-by-step first-run screen sequence is published. It is unclear whether notification or microphone permission is requested up front on mobile.

### Appendix: settings menus (for spec copying) [runtime docs, unofficial, Sep 25 snapshot]
Source: [client-surfaces.md](https://github.com/win4r/MuseAI-Skills/blob/main/home/hatch/docs/client-surfaces.md)
- **iOS Settings order**: Subscription card, then Connectors · Devices · Wallet · Secure credentials store · Permissions · Messaging channels · Encryption (confidential only) · Notifications · Appearance · Data controls · Report an issue · Help & support · Legal info · Accounts Center · Log out.
- **Android adds**: App lock (biometric) · Set as default assistant (needed to send texts) · Add to home screen. Devices has a Location choice: "Never, When chatting, or Always".
- **Web Settings panes**: General (Accounts Center, Appearance, Usage, Language) · Messaging Channels · Devices · Connectors · Wallet · Secure credentials store · Permissions · Data Controls · Encryption · Help & Support · Legal info · Log out.
- **Secure credentials store**: each saved login has an **Agent permissions** choice, "use it automatically" or "always ask first".
