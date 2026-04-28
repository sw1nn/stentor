default:
    @just --list

release type='auto':
    cog bump --{{type}}
