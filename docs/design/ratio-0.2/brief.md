# MASTER PROMPT — Transform My Application Into a World-Class Product

## Your role and mission

You are an elite product designer, design engineer, UX architect, and frontend specialist operating at the level of the best teams at Linear, Vercel, Raycast, and other exceptionally crafted software products.

You have been given an existing application that I am dissatisfied with visually.

**Your mission is to radically improve its design, usability, consistency, and overall craftsmanship until it feels like an expensive, professionally designed application built by an exceptional human design team.**

My application is a project and issue management tool primarily built for **solo software developers**, inspired by Linear.

I want it to feel as polished as Linear, potentially better for its intended audience, but with its own identity.

My biggest concern is avoiding the generic, instantly recognisable appearance of AI-generated interfaces.

This is not a request for a quick CSS refresh. It is a serious product-design project.

## 1. Operating mode: Deep, autonomous design work

**Quality is the primary objective. Speed is secondary.**

I explicitly authorise you to spend substantial time researching, exploring, implementing, testing, and refining the interface.

Do not optimise for minimum effort, minimum tokens, or the fastest possible completion.

Do not stop after making the first version look reasonably good.

I want you to:

1. Understand the application and its existing functionality.
2. Research exceptional real-world software interfaces.
3. Identify why the current design feels weak.
4. Explore multiple genuinely different visual directions.
5. Establish a coherent design language.
6. Implement the strongest direction throughout the application.
7. Open the application and inspect actual rendered results.
8. Identify everything that still looks generic, awkward, or inconsistent.
9. Improve those problems.
10. Repeat visual evaluation and refinement until the design passes a demanding quality review.

Work autonomously. Make informed design decisions without repeatedly asking me to approve minor details.

Treat yourself as the design owner responsible for the final result, not a code generator completing isolated instructions.

## 2. Research before implementation

Before making major changes, examine the design of exceptional productivity applications.

**Primary reference: Linear**

Study its actual product interface, not just its marketing website.

Read these official design articles:

https://linear.app/now/behind-the-latest-design-refresh

https://linear.app/now/how-we-redesigned-the-linear-ui

https://linear.app/now/a-design-reset

Also review:

https://vercel.com/design/guidelines

https://refactoringui.com/

Study the following aspects of successful products:

- Visual hierarchy and information density
- Typography, text contrast, and baseline alignment
- Sidebar and application-shell architecture
- List rows and issue-management interfaces
- Borders, dividers, surfaces, and subtle depth
- Navigation, filtering, search, and keyboard interactions
- Menus, popovers, dialogs, and details panels
- Hover, selected, focused, and active states
- Empty, loading, error, and populated states
- Responsive behaviour and accessibility
- Micro-interactions and perceived responsiveness

Identify the principles behind these decisions.

**Do not simply copy screenshots or reproduce Linear's branding.**

The objective is to understand *why* these applications feel so refined and translate those principles into a distinct product.

If external browsing is unavailable, use your existing design knowledge and available local references. Never pretend research was performed.

## 3. Begin with a ruthless audit

Inspect the entire repository and understand the frontend architecture before rewriting anything.

Run the existing application if the environment allows it.

Explore all significant existing pages, components, navigation states, and user workflows.

Capture baseline screenshots where possible.

Evaluate the current interface as if you were an external senior product designer reviewing it for release.

Look for:

- Generic AI-generated layouts
- Weak information hierarchy
- Inconsistent spacing
- Unnecessarily large cards
- Excessive whitespace
- Awkward typography
- Poor alignment
- Inconsistent border radii
- Too many borders or shadows
- Poor colour relationships
- Oversized icons
- Inconsistent component sizing
- Unclear primary and secondary actions
- Weak navigation
- Unnecessary interface complexity
- Components that feel disconnected
- Visually unbalanced screens
- Areas where the interface feels like a template

Determine the root cause of each problem.

Do not merely patch individual symptoms. Fix underlying design-system and layout problems.

Preserve existing functional behaviour, data handling, backend integration, and routing unless a change is genuinely necessary.

## 4. Design direction

The desired aesthetic is:

**Monochromatic. Minimal. Precise. Dense where useful. Quietly premium. Functional. Timeless.**

Imagine the attention to detail of Linear combined with the clarity of a beautifully crafted native desktop application.

The interface should feel like a serious tool used daily by professional developers.

### Colour system

Prefer a dark-first, primarily black-and-white interface.

Use carefully chosen shades of black, charcoal, graphite, neutral grey, and off-white.

Avoid using identical black backgrounds everywhere. Create subtle distinctions between the application shell, navigation sidebar, main content, elevated panels, and dialogs.

Use contrast and luminance to establish hierarchy instead of relying on colourful decoration.

Reserve limited accent colours for meaningful states such as priorities, statuses, errors, or genuinely important actions.

Avoid colourful dashboards.

The final palette must be tested visually and for accessibility rather than chosen arbitrarily.

### Typography

Treat typography as one of the most important parts of the redesign.

Choose one excellent interface typeface and a suitable monospace font where necessary.

Consider Inter, Geist, or similarly refined fonts if compatible with the project.

Establish a deliberate typographic hierarchy for:

- Page titles
- Navigation labels
- Section headings
- Issue titles
- Metadata
- Supporting text
- Button labels
- Keyboard shortcuts
- Numerical information

Use restrained font sizes, sensible font weights, refined letter spacing, and deliberate line heights.

Avoid oversized dashboard headings and excessive bold text.

The application should feel precise, not loud.

### Spacing and layout

Create a consistent spacing system with reusable tokens.

Use intentional alignment rather than arbitrary margins.

The main working area should receive most of the visual attention.

Navigation should be clearly accessible but visually subordinate.

Use whitespace to separate concepts, not as a substitute for structure.

Prefer content-first layouts and subtle separators over enclosing every piece of information in a card.

Avoid excessively wide reading areas, inconsistent padding, and unbalanced empty space.

### Borders, radius, and elevation

Use restrained corner radii.

Prefer subtle borders and carefully controlled surface contrast.

Shadows should be limited and purposeful.

Avoid excessive rounded rectangles, floating panels, thick outlines, and unnecessary elevation.

The application should look engineered, not decorated.

## 5. Absolute anti-AI-slop requirements

This section is extremely important.

You must actively identify and remove common AI-generated design patterns.

**Do not introduce:**

- Generic purple, blue, or multicolour gradients
- Glowing blobs or decorative background orbs
- Glassmorphism everywhere
- Excessive backdrop blur
- Giant rounded cards
- Card grids used where compact lists would work better
- Generic three-column analytics dashboards
- Huge welcome banners
- Unnecessary hero sections inside the application
- Random colourful icons
- Decorative emojis
- Gratuitous shadows
- Unnecessary charts, statistics, or progress indicators
- Excessive badges and pills
- Generic SaaS dashboard layouts
- Arbitrary animations
- Repetitive layouts copied from component libraries
- Large empty areas created just to look minimalist
- Marketing-style copy inside productivity workflows
- Placeholder features or fake data presented as actual functionality

Do not use a standard shadcn/ui configuration and call it finished.

You may use existing component libraries for accessibility and behaviour, but the final visual system must have its own coherent styling.

**Avoid producing something that looks like an AI-generated clone of Linear.**

The interface needs personality through precise decisions, not decorative novelty.

Every visual element must justify its presence.

If a component can be removed without harming usability, strongly consider removing it.

## 6. Design for solo developers, not enterprise teams

My product is intended primarily for individual software developers.

This is our opportunity to improve upon Linear for a more focused use case.

Prioritise the experience of an individual planning, organising, and completing software development work.

The core experience should make it effortless to understand:

- What I am currently working on
- What needs my attention
- Which issues belong to which projects
- What I should work on next
- How my projects are progressing

Where supported by existing features, optimise creating issues, navigating projects, editing details, filtering lists, and managing priorities.

Do not introduce enterprise complexity, unnecessary collaboration features, or distracting dashboards.

Do not invent new product features merely to populate the redesigned interface.

Use progressive disclosure: keep the common workflow simple while making advanced controls accessible when needed.

The ideal experience is powerful without feeling complicated.

## 7. Explore before committing

Before finalising a visual direction, create and evaluate at least three meaningfully different design approaches.

These should be distinct concepts, not three nearly identical colour variations.

For example, explore:

**Direction A — Precision Minimalism**

An exceptionally clean, dense, typography-driven interface with almost invisible structural boundaries.

**Direction B — Refined Desktop Application**

A slightly more tactile interface inspired by premium native macOS applications, with carefully controlled surface depth and highly polished controls.

**Direction C — Modern Developer Workspace**

A sophisticated interface combining compact information architecture, technical clarity, excellent keyboard navigation, and a distinctive monochromatic identity.

Evaluate how each direction serves the actual workflows.

Select the strongest direction or deliberately combine compatible elements.

Explain the reasoning briefly in your design notes, then implement it consistently.

Do not combine styles indiscriminately.

## 8. Build a proper design system

Before extensively changing individual screens, establish or refine the application's design foundation.

Create a maintainable system covering:

**Design tokens:** backgrounds, elevated surfaces, text colours, borders, interaction states, spacing, sizing, radii, typography, and motion.

**Core components:** buttons, inputs, selects, search fields, navigation items, tabs, filters, issue rows, badges, dialogs, popovers, tooltips, and relevant existing components.

**Component states:** default, hovered, active, selected, focused, disabled, loading, and error.

**Layout primitives:** sidebar, app header, content header, content containers, panels, compact lists, and responsive navigation.

Avoid repeated magic numbers and one-off CSS exceptions.

Do not unnecessarily rewrite working components or add dependencies when existing architecture can support the design.

The system must be consistent enough that future features naturally inherit the same visual language.

## 9. Redesign the real application

Apply the design across the existing application, not just its most attractive page.

Pay special attention to the navigation sidebar, main workspace, project views, issue lists, issue details, forms, settings, and other important routes that actually exist.

### Application shell

Create a highly refined, stable overall structure.

The sidebar should feel compact and thoughtfully organised.

The content area must have a clear visual relationship to navigation.

Ensure consistent positioning of global controls, page titles, and view-specific actions.

### Issue and task lists

Treat these as foundational parts of the product.

Optimise rows for fast scanning.

Establish a careful hierarchy between issue title, status, priority, project, and supporting metadata.

Ensure selection states, hover states, density, and alignment are meticulously designed.

Avoid turning every issue into a large card.

### Detail views

Use clean typography and intuitive information grouping.

Make descriptions, properties, and editable fields easy to work with.

Metadata should be available without competing with the primary content.

### Dialogs and menus

Make them feel like part of the same product.

Carefully tune their dimensions, padding, item heights, borders, focus states, and keyboard behaviour.

A premium interface often reveals its quality most clearly in these small components.

## 10. Micro-interactions and usability

The application must feel excellent to use, not just look excellent in screenshots.

Implement refined, understated interactions where appropriate.

Prefer subtle hover transitions, clear selection feedback, predictable keyboard navigation, and fast responses.

Animations should communicate state changes rather than draw attention to themselves.

Support reduced-motion preferences.

Use semantic controls, visible keyboard focus, accessible labels, and sufficient contrast.

Make important actions discoverable without filling every screen with buttons.

Preserve or improve existing workflows.

No dead buttons, broken menus, fake controls, or visual-only functionality.

## 11. Mandatory visual iteration loop

This is one of the most important requirements.

**Do not evaluate the redesign exclusively by reading the source code.**

Run the application and inspect its actual visual appearance.

If browser automation or screenshot tools are available, use them extensively.

Test at representative desktop, laptop, and mobile viewport sizes, including approximately 1440×900, 1280×800, and 390×844.

For important screens, inspect normal, empty, dense, and interactive states where applicable.

Perform at least three meaningful visual-review cycles for the core application experience, if the tools permit.

For each cycle:

1. Capture the rendered result.
2. Examine it critically.
3. Identify the most damaging visual and usability problems.
4. Fix those problems in the implementation.
5. Capture the result again.
6. Compare it with the previous iteration.
7. Check whether the changes improved the overall system rather than one isolated screenshot.

During every review ask:

Does this look like a deliberately designed professional product?

Does any part still look like a generic AI-generated template?

Is the visual hierarchy immediately understandable?

Are the smallest details consistent?

Is the interface unnecessarily complicated?

Would removing something improve it?

Does the design work with real, dense content?

Does the product feel distinct from Linear while matching its level of craftsmanship?

**Do not consider the first attractive screenshot sufficient evidence of quality.**

If screenshots or a browser cannot be accessed, be explicit about the limitation and perform the strongest available code-level and layout review instead. Never fabricate visual-test results.

## 12. Preserve engineering quality

While redesigning, preserve the application's actual purpose.

Do not break existing features, routes, API contracts, data persistence, or backend integrations.

Do not silently replace functional interfaces with static mockups.

Avoid unnecessary framework migrations and excessive new dependencies.

Use the existing architectural conventions where sensible.

Refactor when it provides a clear benefit to consistency, usability, accessibility, or maintainability.

Check the existing tests, type checking, linting, and build process.

Run the relevant validation tools.

Fix regressions caused by your changes.

The finished work must be a real, maintainable application.

## 13. Final quality standard

Before declaring completion, review the entire result as a demanding design director.

Judge the application on:

1. Visual hierarchy
2. Typography
3. Spacing and alignment
4. Colour and contrast
5. Information density
6. Consistency
7. Navigation clarity
8. Micro-interactions
9. Accessibility
10. Originality and freedom from generic AI aesthetics

For each category, give a justified quality assessment.

Any weak category should trigger another targeted improvement pass when feasible.

Do not inflate scores or claim perfection.

The objective is not to satisfy a superficial checklist. It is to deliver a genuinely excellent product.

### Deliverables

When implementation is complete, provide:

- A summary of the visual problems you discovered
- The design direction you selected and why
- The actual changes implemented
- The design-system improvements
- Before-and-after screenshots where available
- The visual refinement iterations performed
- Validation and test results
- Any remaining issues or limitations

Do not substitute a design document for actual implementation.

## Final instruction

I would rather have an exceptionally well-designed, cohesive application than a large quantity of quickly generated UI changes.

Be ambitious about craftsmanship and conservative about unnecessary complexity.

Do not rush. Do not add decoration to create an illusion of quality. Do not settle for the default appearance of any framework or component library.

**Make every pixel feel intentional.**

Use Linear as the benchmark for discipline, but build an identity that makes sense for this application.

The finished product should feel like it was crafted through serious design thinking, thoughtful engineering, and repeated visual refinement—not generated from a single prompt.

**Inspect. Design. Implement. Render. Critique. Refine. Validate.**

Now begin with the existing repository and carry the redesign through to a working, thoroughly reviewed implementation.