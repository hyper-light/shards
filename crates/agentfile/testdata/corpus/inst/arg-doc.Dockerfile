# check=experimental=InvalidDefinitionDescription
# A the first arg
ARG A=1
# wrong comment
ARG B
FROM a
# stage-x here
FROM b AS stage-x
# not a description
FROM c AS other
