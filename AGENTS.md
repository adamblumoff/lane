The goal of this application is to allow agents to work on the same file at the same time in different "lanes" asynchronously without making multiple copies of the same file.

This is intended to replace the git worktree flow that is very annoying and cumbersome. 

This is intended to go lower level into the "file" level. 

This product is still in pre-alpha. Every change should be destructive, no legacy or backwards compatible bullshit, this application has no users or releases, treat it as such. If you add backwards compatibility, I will go apeshit on your dumbass. 

Make sure to continuously dogfood lane while building so that you can catch edge cases and stuff along the way.

Don't just patch stuff all willy nilly, take a long-term maintainable approach to fixes.

After every code change, tell me what actually improved, make it quantitative if possible.

Test code should be on different files then src code unless there is a really good justification.

Automatic tests should copy flows of manual verification that are important enough to keep running in the future.

Don't roll your own UI if you don't have to, use some other premade popular package that fits the spec.
